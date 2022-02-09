use regex::Regex;
use serenity::{
    async_trait,
    model::{channel::Message, id::GuildId, user::User},
    prelude::*,
    CacheAndHttp,
};
use std::{
    collections::VecDeque,
    env,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::{sync::mpsc, time::Instant};

// Names of channels for which the bot is enabled
const ENABLED_CHANNELS: &[&str] = &["bot-stuff"];

// Minimum duration between dispensation of tokens
const RATE_LIMIT: Duration = Duration::from_secs(60);

// Maximum number of addresses per message (the rest will be told to try again)
const MAX_ADDRESSES_PER_MESSAGE: usize = 1;

struct Handler {
    address_regex: Regex,
    send_history: Arc<Mutex<VecDeque<(User, Instant)>>>,
}

impl Handler {
    fn new() -> Self {
        Handler {
            // Match penumbra testnet addresses (any version)
            address_regex: Regex::new(r"penumbrav\dt1[qpzry9x8gf2tvdw0s3jn54khce6mua7l]{126}")
                .unwrap(),
            send_history: Arc::new(Mutex::new(VecDeque::new())),
        }
    }
}

#[async_trait]
impl EventHandler for Handler {
    async fn message(&self, ctx: Context, message: Message) {
        // Don't trigger on messages we ourselves send, or when a message is in a channel we're not
        // supposed to interact within
        if message.author.id == ctx.cache.current_user().await.id
            || !ENABLED_CHANNELS.contains(
                &message
                    .channel_id
                    .name(&ctx)
                    .await
                    .unwrap_or_else(String::new)
                    .as_str(),
            )
        {
            tracing::trace!("detected message from invalid channel or from ourselves");
            return;
        }

        // Prune the send history of all expired rate limit timeouts
        {
            tracing::trace!("pruning send history");
            // scoped to prevent deadlock on send_history
            let mut send_history = self.send_history.lock().unwrap();
            while let Some((user, last_fulfilled)) = send_history.front() {
                if last_fulfilled.elapsed() >= RATE_LIMIT {
                    tracing::debug!(?user, ?last_fulfilled, "rate limit expired");
                    send_history.pop_front();
                } else {
                    break;
                }
            }
            tracing::trace!("finished pruning send history");
        }

        // If the message author was in the send history, don't send them tokens
        let last_fulfilled = self
            .send_history
            .lock()
            .unwrap()
            .iter()
            .find(|(user, _)| user == &message.author)
            .map(|(_, last_fulfilled)| *last_fulfilled);

        let queue_message = if let Some(last_fulfilled) = last_fulfilled {
            tracing::info!(user_name = ?message.author.name, user_id = ?message.author.id.to_string(), ?last_fulfilled, "rate-limited user");
            AddressQueueMessage::RateLimit {
                last_fulfilled,
                message,
            }
        } else {
            // Push the user into the send history queue for rate-limiting in the future
            tracing::trace!(user = ?message.author, "pushing user into send history");
            self.send_history
                .lock()
                .unwrap()
                .push_back((message.author.clone(), Instant::now()));

            // Collect all the matches into a struct, bundled with the original message
            tracing::trace!("collecting addresses from message");
            let addresses: Vec<String> = self
                .address_regex
                .find_iter(&message.content)
                .map(|m| m.as_str().to_string())
                .collect();

            // If no addresses were found, don't bother sending the message to the queue
            if addresses.is_empty() {
                tracing::trace!("no addresses found in message");
                return;
            }

            tracing::trace!(addresses = ?addresses, "sending addresses to worker");
            AddressQueueMessage::Dispense { addresses, message }
        };

        // Send the message to the queue, to be processed asynchronously
        tracing::trace!("sending message to worker queue");
        ctx.data
            .read()
            .await
            .get::<AddressQueue>()
            .expect("address queue exists")
            .send(queue_message)
            .await
            .expect("send to queue always succeeds");
    }

    async fn cache_ready(&self, ctx: Context, guilds: Vec<GuildId>) {
        for guild_id in guilds {
            let server_name = guild_id
                .name(&ctx.cache)
                .await
                .unwrap_or_else(|| "[unknown]".to_string());
            tracing::info!(
                ?server_name,
                server_id = ?guild_id.to_string(),
                "connected to server"
            );
        }
    }
}

/// `TypeMap` key for the address queue.
struct AddressQueue;

/// `TypeMap` value for the sender end of the address queue.
#[derive(Debug, Clone)]
enum AddressQueueMessage {
    Dispense {
        /// The originating message that contained these addresses.
        message: Message,
        /// The addresses matched in the originating message.
        addresses: Vec<String>,
    },
    RateLimit {
        /// The originating message that resulted in this rate-limit.
        message: Message,
        /// The last time this request was fulfilled for the user.
        last_fulfilled: Instant,
    },
}

/// Associate the `AddressQueue` key with an `mpsc::Sender` for `AddressQueueMessage`s in the `TypeMap`.
impl TypeMapKey for AddressQueue {
    type Value = mpsc::Sender<AddressQueueMessage>;
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // Make a new client using a token set by an environment variable, with our handlers
    let mut client = Client::builder(&env::var("DISCORD_TOKEN")?)
        .event_handler(Handler::new())
        .await?;

    // Get the cache and http part of the client, for use in dispatching replies
    let cache_http = client.cache_and_http.clone();

    // Put the sending end of the address queue into the global TypeMap
    let (tx, mut rx) = mpsc::channel(100);
    client.data.write().await.insert::<AddressQueue>(tx);

    // Spawn a task to handle the address queue
    tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            match m {
                AddressQueueMessage::RateLimit {
                    last_fulfilled,
                    message,
                } => {
                    message
                        .reply_ping(
                            &cache_http,
                            format!(
                                "Please wait for another {} before requesting more tokens. Thanks!",
                                humantime::Duration::from(RATE_LIMIT - last_fulfilled.elapsed())
                            ),
                        )
                        .await
                        .map(|_| ())
                        .unwrap_or_else(|e| {
                            tracing::error!(?e, "failed to reply about rate limit")
                        });
                }
                AddressQueueMessage::Dispense { addresses, message } => {
                    // Determine which addresses we are going to send to, and which we are going to report
                    // as "too many"
                    let (dispense_to_addresses, remaining_addresses) =
                        addresses.split_at(MAX_ADDRESSES_PER_MESSAGE);

                    // Track addresses to which we successfully dispensed tokens
                    let mut succeeded_addresses = Vec::<&String>::new();

                    // Track addresses (and associated errors) which we tried to send tokens to, but failed
                    let mut failed_addresses = Vec::<(&String, String)>::new();

                    for addr in dispense_to_addresses {
                        // Reply to the originating message with the address
                        tracing::info!(user_name = ?message.author.name, user_id = ?message.author.id.to_string(), address = ?addr, "sending tokens");

                        // TODO: Invoke `pcli tx send` to dispense some random coins to the address, then
                        // wait until `pcli balance` indicates that the transaction has been confirmed.
                        // While this is happening, use the typing indicator API to show that something is
                        // happening.
                        let result = Ok::<_, anyhow::Error>(());

                        match result {
                            Ok(()) => succeeded_addresses.push(addr),
                            Err(e) => failed_addresses.push((addr, e.to_string())),
                        }
                    }

                    // Compose a response to the user summarizing the actions taken
                    let mut response = String::new();
                    if !succeeded_addresses.is_empty() {
                        response.push_str("I've sent tokens to the following addresses:");
                        for addr in succeeded_addresses {
                            response.push_str(&format!("\n`{}`", addr));
                        }
                    }
                    if !failed_addresses.is_empty() {
                        response.push_str("I've failed to send tokens to the following addresses:");
                        for (addr, error) in failed_addresses {
                            response.push_str(&format!("\n`{}` (error: {})", addr, error));
                        }
                    }
                    if !remaining_addresses.is_empty() {
                        response.push_str(&format!(
                            "\nI can only dispense tokens to {} address per request, \
                            so please try again later with the following remaining addresses:",
                            MAX_ADDRESSES_PER_MESSAGE
                        ));
                        for addr in remaining_addresses {
                            response.push_str(&format!("\n`{}`", addr));
                        }
                    }

                    // Reply with a summary of what occurred
                    message
                        .reply_ping(&cache_http, response)
                        .await
                        .map(|_| ())
                        .unwrap_or_else(|e| {
                            tracing::error!(?e, "failed to reply about dispensing tokens")
                        });
                }
            }
        }
    });

    // Start the client
    client.start().await?;

    Ok(())
}
