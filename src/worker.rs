use std::{borrow::Borrow, sync::Arc, time::Duration};

use serenity::{
    model::{channel::Message, id::RoleId},
    prelude::Mentionable,
    CacheAndHttp,
};
use tokio::{sync::mpsc, time::Instant};

use super::action::Action;

pub struct Worker {
    /// Maximum number of addresses to handle per message.
    max_addresses_per_message: usize,
    /// Actions to perform.
    actions: mpsc::Receiver<Action>,
    /// Cache and http for dispatching replies.
    cache_http: Arc<CacheAndHttp>,
    /// Administrator role to mention when errors occur.
    admin_role: Option<RoleId>,
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
            admin_role: None,
        }
    }

    pub async fn run(mut self) {
        while let Some(m) = self.actions.recv().await {
            match m {
                Action::RateLimit {
                    last_fulfilled,
                    rate_limit,
                    message,
                } => self.rate_limit(message, rate_limit, last_fulfilled).await,
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
        let response =
            self.dispense_summary(&succeeded_addresses, &failed_addresses, remaining_addresses);
        self.reply(message, response).await
    }

    fn dispense_summary<'a>(
        &self,
        succeeded_addresses: &[&'a String],
        failed_addresses: &[(&'a String, String)],
        remaining_addresses: &[String],
    ) -> String {
        let succeeded_addresses = succeeded_addresses.borrow();
        let failed_addresses = failed_addresses.borrow();
        let remaining_addresses = remaining_addresses.borrow();

        let mut response = String::new();

        if !succeeded_addresses.is_empty() {
            response.push_str("Successfully sent tokens to the following addresses:\n");
            for addr in succeeded_addresses {
                response.push_str(&format!("`{}`\n", addr));
            }
        }

        if !failed_addresses.is_empty() {
            response.push_str("Failed to send tokens to the following addresses:\n");
            for (addr, error) in failed_addresses {
                response.push_str(&format!("`{}` (error: {})\n", addr, error));
            }
            response.push_str(&format!(
                "{}: you may want to investigate this error :)",
                self.admin_role
                    .map(|role_id| role_id.mention().to_string())
                    .unwrap_or_else(|| "Server administrator(s)".to_string())
            ))
        }

        if !remaining_addresses.is_empty() {
            response.push_str("\nThe following addresses were not sent tokens because you have already requested them recently:\n");
            for addr in remaining_addresses {
                response.push_str(&format!("`{}`\n", addr));
            }
        }

        response
    }

    async fn rate_limit(
        &mut self,
        message: impl Borrow<Message>,
        rate_limit: Duration,
        last_fulfilled: Instant,
    ) {
        let response = format!(
            "Please wait for another {} before requesting more tokens. Thanks!",
            humantime::Duration::from(rate_limit - last_fulfilled.elapsed())
        );
        self.reply(message, response).await
    }
}
