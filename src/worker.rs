use std::{borrow::Borrow, sync::Arc, time::Duration};

use serenity::{model::channel::Message, prelude::Mentionable, CacheAndHttp};
use tokio::{sync::mpsc, time::Instant};

use super::action::Action;

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
        actions: mpsc::Receiver<Action>,
        cache_http: Arc<CacheAndHttp>,
    ) -> Self {
        Worker {
            max_addresses_per_message,
            actions,
            cache_http,
        }
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

    async fn dispense(&mut self, message: impl Borrow<Message>, addresses: Vec<String>) {
        let message = message.borrow();

        // Determine which addresses we are going to send to, and which we are going to report
        // as "too many"
        let (dispense_to_addresses, remaining_addresses) =
            addresses.split_at(self.max_addresses_per_message);

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

        // Reply with a summary of what occurred
        self.reply_dispense_summary(
            &succeeded_addresses,
            &failed_addresses,
            remaining_addresses,
            message,
        )
        .await;
    }

    async fn reply_dispense_summary<'a>(
        &mut self,
        succeeded_addresses: &[&'a String],
        failed_addresses: &[(&'a String, String)],
        remaining_addresses: &[String],
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
        _addresses: Vec<String>,
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
