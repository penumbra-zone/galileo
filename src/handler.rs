use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use regex::Regex;
use serenity::{
    async_trait,
    client::{Context, EventHandler},
    model::{
        channel::Message,
        id::{GuildId, UserId},
    },
};
use tokio::time::{Duration, Instant};

use super::action::{Action, ActionQueue};

#[derive(Debug, Clone, Copy)]
pub enum Notified {
    /// The user has been notified of the rate-limit.
    Already,
    /// The user has not been notified of the rate-limit.
    NotYet,
}

pub struct Handler {
    enabled_channels: Vec<String>,
    rate_limit: Duration,
    address_regex: Regex,
    send_history: Arc<Mutex<VecDeque<(UserId, Instant, Notified)>>>,
}

impl Handler {
    pub fn new(rate_limit: Duration, enabled_channels: Vec<String>) -> Self {
        Handler {
            rate_limit,
            enabled_channels,
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
            || !self.enabled_channels.contains(
                &message
                    .channel_id
                    .name(&ctx)
                    .await
                    .unwrap_or_else(String::new),
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
            while let Some((user, last_fulfilled, _)) = send_history.front() {
                if last_fulfilled.elapsed() >= self.rate_limit {
                    tracing::debug!(?user, ?last_fulfilled, "rate limit expired");
                    send_history.pop_front();
                } else {
                    break;
                }
            }
            tracing::trace!("finished pruning send history");
        }

        // If the message author was in the send history, don't send them tokens
        let rate_limited = self
            .send_history
            .lock()
            .unwrap()
            .iter_mut()
            .find(|(user, _, _)| *user == message.author.id)
            .map(|(_, last_fulfilled, notified)| {
                (
                    *last_fulfilled,
                    // We set notified to `Already` to prevent replying about the rate limit again:
                    std::mem::replace(notified, Notified::Already),
                )
            });

        let queue_message = if let Some((last_fulfilled, notified)) = rate_limited {
            tracing::info!(
                user_name = ?message.author.name,
                ?notified,
                user_id = ?message.author.id.to_string(),
                ?last_fulfilled,
                "rate-limited user"
            );

            // If we already notified the user, don't reply again
            if let Notified::Already = notified {
                return;
            }

            Action::RateLimit {
                rate_limit: self.rate_limit,
                last_fulfilled,
                message,
            }
        } else {
            // Push the user into the send history queue for rate-limiting in the future
            tracing::trace!(user = ?message.author, "pushing user into send history");
            self.send_history.lock().unwrap().push_back((
                message.author.id,
                Instant::now(),
                Notified::NotYet,
            ));

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
            Action::Dispense { addresses, message }
        };

        // Send the message to the queue, to be processed asynchronously
        tracing::trace!("sending message to worker queue");
        ctx.data
            .read()
            .await
            .get::<ActionQueue>()
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
