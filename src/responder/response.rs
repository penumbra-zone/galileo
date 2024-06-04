use std::fmt::Write;

use penumbra_keys::Address;
use penumbra_txhash::TransactionId;
use serenity::{client::Cache, model::id::GuildId};

/// The response from a request to dispense tokens to a set of addresses.
#[derive(Debug, Clone)]
pub struct Response {
    /// The addresses that were successfully dispensed tokens.
    pub(crate) succeeded: Vec<(Address, TransactionId)>,
    /// The addresses that failed to be dispensed tokens, accompanied by a string describing the
    /// error.
    pub(crate) failed: Vec<(Address, String)>,
    /// The addresses that couldn't be parsed.
    pub(crate) unparsed: Vec<String>,
    /// The addresses that were limited from being dispensed tokens because only a certain number
    /// are permitted to be given tokens per message.
    pub(crate) remaining: Vec<Address>,
}

impl Response {
    /// Returns the addresses that were successfully dispensed tokens.
    pub fn succeeded(&self) -> &[(Address, TransactionId)] {
        &self.succeeded
    }

    /// Returns the addresses that failed to be dispensed tokens, accompanied by a string describing
    /// the error.
    pub fn failed(&self) -> &[(Address, String)] {
        &self.failed
    }

    /// Returns the addresses that couldn't be parsed.
    pub fn unparsed(&self) -> &[String] {
        &self.unparsed
    }

    /// Returns the addresses that were limited from being dispensed tokens because only a certain
    /// number are permitted to be given tokens per message.
    pub fn remaining(&self) -> &[Address] {
        &self.remaining
    }

    /// Returns `true` only if all addresses were successfully dispensed tokens.
    pub fn complete_success(&self) -> bool {
        self.failed.is_empty() && self.unparsed.is_empty() && self.remaining.is_empty()
    }

    /// Returns `false` only if no addresses were successfully dispensed tokens.
    pub fn complete_failure(&self) -> bool {
        self.succeeded.is_empty() && !self.complete_success()
    }

    /// Construct a string summarizing the response.
    ///
    /// This requires [`Cache`] and a [`GuildId`] so that it can mention the administrator role(s)
    /// of the server if an error occurred.
    pub async fn summary(&self, _cache: impl AsRef<Cache>, _guild_id: GuildId) -> String {
        let mut response = String::new();

        if !self.succeeded.is_empty() {
            response.push_str("Successfully sent tokens to the following addresses:");
            for (addr, id) in self.succeeded.iter() {
                write!(
                    response,
                    "\n`{}`\ntry `pcli v tx {}`\nor visit https://app.testnet.penumbra.zone/#/tx/{}",
                    addr.display_short_form(),
                    id,
                    id,
                )
                .unwrap();
            }
        }

        if !self.failed.is_empty() {
            response.push_str("Failed to send tokens to the following addresses:");
            for (addr, error) in self.failed.iter() {
                write!(response, "\n`{}` (error: {})", addr, error).unwrap();
            }

            write!(
                response,
                "\nadmins: you may want to investigate this error :)",
            )
            .unwrap();
        }

        if !self.unparsed.is_empty() {
            response.push_str(
                "\nThe following _look like_ Penumbra addresses, \
                but are invalid (maybe a typo or old address version?):",
            );
            for addr in self.unparsed.iter() {
                write!(response, "\n`{}`", addr).unwrap();
            }
        }

        if !self.remaining.is_empty() {
            write!(
                response,
                "\nI'm only allowed to send tokens to addresses {} at a time; \
                try again later to get tokens for the following addresses:",
                self.succeeded.len(),
            )
            .unwrap();
            for addr in self.remaining.iter() {
                write!(response, "\n`{}`", addr).unwrap();
            }
        }

        response
    }
}
