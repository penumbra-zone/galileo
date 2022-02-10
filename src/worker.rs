use std::{borrow::Borrow, sync::Arc, time::Duration};

use penumbra_crypto::Address;
use serenity::{model::channel::Message, prelude::Mentionable, CacheAndHttp};
use tokio::{sync::mpsc, time::Instant};

use super::action::{Action, AddressOrAlmost};

pub struct Worker {
    /// Maximum number of addresses to handle per message.
    max_addresses_per_message: usize,
    /// Actions to perform.
    actions: mpsc::Receiver<Action>,
    /// Cache and http for dispatching replies.
    cache_http: Arc<CacheAndHttp>,
}

impl Worker {
    pub fn new(
        max_addresses_per_message: usize,
        cache_http: Arc<CacheAndHttp>,
        buffer_size: usize,
    ) -> (mpsc::Sender<Action>, Self) {
        let (tx, rx) = mpsc::channel(buffer_size);
        (
            tx,
            Worker {
                max_addresses_per_message,
                actions: rx,
                cache_http,
            },
        )
    }

    pub async fn run(mut self) {
        while let Some(m) = self.actions.recv().await {
            match m {
                Action::RateLimit {
                    last_fulfilled,
                    rate_limit,
                    addresses,
                    message,
                } => {
                    self.rate_limit(message, rate_limit, last_fulfilled, addresses)
                        .await
                }
                Action::Dispense { addresses, message } => self.dispense(message, addresses).await,
            }
        }
    }

    async fn reply(&mut self, message: impl Borrow<Message>, response: impl Borrow<str>) {
        message
            .borrow()
            .reply_ping(&self.cache_http, response.borrow())
            .await
            .map(|_| ())
            .unwrap_or_else(|e| tracing::error!(error = ?e, "failed to reply"));
    }

    async fn dispense(
        &mut self,
        message: impl Borrow<Message>,
        mut addresses: Vec<AddressOrAlmost>,
    ) {
        let message = message.borrow();

        // Track addresses to which we successfully dispensed tokens
        let mut succeeded_addresses = Vec::<Address>::new();

        // Track addresses (and associated errors) which we tried to send tokens to, but failed
        let mut failed_addresses = Vec::<(Address, String)>::new();

        // Track addresses which couldn't be parsed
        let mut unparsed_addresses = Vec::<String>::new();

        // Extract up to the maximum number of permissible valid addresses from the list
        let mut count = 0;
        while count <= self.max_addresses_per_message {
            count += 1;
            match addresses.pop() {
                Some(AddressOrAlmost::Address(addr)) => {
                    // Reply to the originating message with the address
                    tracing::info!(user_name = ?message.author.name, user_id = ?message.author.id.to_string(), address = ?addr, "sending tokens");

                    // TODO: Invoke `pcli tx send` to dispense some random coins to the address, then
                    // wait until `pcli balance` indicates that the transaction has been confirmed.
                    // While this is happening, use the typing indicator API to show that something is
                    // happening.
                    let result = Ok::<_, anyhow::Error>(());

                    match result {
                        Ok(()) => succeeded_addresses.push(*addr),
                        Err(e) => failed_addresses.push((*addr, e.to_string())),
                    }
                }
                Some(AddressOrAlmost::Almost(addr)) => {
                    unparsed_addresses.push(addr);
                }
                None => break,
            }
        }

        // Separate the rest of the list into unparsed and remaining valid ones
        let mut remaining_addresses = Vec::<Address>::new();
        for addr in addresses {
            match addr {
                AddressOrAlmost::Address(addr) => remaining_addresses.push(*addr),
                AddressOrAlmost::Almost(addr) => unparsed_addresses.push(addr),
            }
        }

        // Reply with a summary of what occurred
        self.reply_dispense_summary(
            &succeeded_addresses,
            &failed_addresses,
            &unparsed_addresses,
            &remaining_addresses,
            message,
        )
        .await;
    }

    async fn reply_dispense_summary<'a>(
        &mut self,
        succeeded_addresses: &[Address],
        failed_addresses: &[(Address, String)],
        unparsed_addresses: &[String],
        remaining_addresses: &[Address],
        message: impl Borrow<Message>,
    ) {
        let succeeded_addresses = succeeded_addresses.borrow();
        let failed_addresses = failed_addresses.borrow();
        let remaining_addresses = remaining_addresses.borrow();

        let mut response = String::new();

        if !succeeded_addresses.is_empty() {
            response.push_str("Successfully sent tokens to the following addresses:");
            for addr in succeeded_addresses {
                response.push_str(&format!("\n`{}`", addr));
            }
        }

        if !failed_addresses.is_empty() {
            response.push_str("Failed to send tokens to the following addresses:\n");
            for (addr, error) in failed_addresses {
                response.push_str(&format!("\n`{}` (error: {})", addr, error));
            }

            // Construct a mention for the admin roles for this server
            let mention_admins = if let Some(guild_id) = message.borrow().guild_id {
                self.cache_http
                    .cache
                    .guild_roles(guild_id)
                    .await
                    .iter()
                    .map(IntoIterator::into_iter)
                    .flatten()
                    .filter(|(_, r)| r.permissions.administrator())
                    .map(|(&id, _)| id)
                    .map(|role_id| role_id.mention().to_string())
                    .collect::<Vec<String>>()
                    .join(" ")
            } else {
                "Admin(s)".to_string()
            };

            response.push_str(&format!(
                "\n{mention_admins}: you may want to investigate this error :)",
            ))
        }

        if !unparsed_addresses.is_empty() {
            response.push_str(
                "\nThe following _look like_ Penumbra addresses, \
                but are invalid (maybe a typo or old address version?):",
            );
            for addr in unparsed_addresses {
                response.push_str(&format!("\n`{}`", addr));
            }
        }

        if !remaining_addresses.is_empty() {
            response.push_str(&format!(
                "\nI'm only allowed to send tokens to addresses {} at a time; \
                try again later to get tokens for the following addresses:",
                self.max_addresses_per_message,
            ));
            for addr in remaining_addresses {
                response.push_str(&format!("\n`{}`", addr));
            }
        }

        self.reply(message, response).await;
    }

    async fn rate_limit(
        &mut self,
        message: impl Borrow<Message>,
        rate_limit: Duration,
        last_fulfilled: Instant,
        _addresses: Vec<AddressOrAlmost>,
    ) {
        let response = format!(
            "Please wait for another {} before requesting more tokens. Thanks!",
            format_remaining_time(last_fulfilled, rate_limit)
        );
        self.reply(message, response).await
    }
}

fn format_remaining_time(last_fulfilled: Instant, rate_limit: Duration) -> String {
    humantime::Duration::from(rate_limit - last_fulfilled.elapsed())
        .to_string()
        .split(' ')
        .take(3)
        .collect::<Vec<_>>()
        .join(" ")
}
