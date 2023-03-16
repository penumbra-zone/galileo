use std::fmt::Write;

use penumbra_crypto::Address;
use penumbra_transaction::Id;
use serenity::{client::Cache, model::id::GuildId, prelude::Mentionable};

/// The response from a request to dispense tokens to a set of addresses.
#[derive(Debug)]
pub struct Response {
    /// The addresses that were successfully dispensed tokens.
    pub(super) succeeded: Vec<(Address, Id)>,
    /// The addresses that failed to be dispensed tokens, accompanied by a string describing the
    /// error.
    pub(super) failed: Vec<(Address, String)>,
    /// The addresses that couldn't be parsed.
    pub(super) unparsed: Vec<String>,
    /// The addresses that were limited from being dispensed tokens because only a certain number
    /// are permitted to be given tokens per message.
    pub(super) remaining: Vec<Address>,
}

impl Response {
    /// Returns the addresses that were successfully dispensed tokens.
    pub fn succeeded(&self) -> &[(Address, Id)] {
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
    pub async fn summary(&self, cache: impl AsRef<Cache>, guild_id: GuildId) -> String {
        /// Construct a mention for the admin roles for this server
        async fn mention_admins(cache: impl AsRef<Cache>, guild_id: GuildId) -> String {
            cache
                .as_ref()
                .guild_roles(guild_id)
                .iter()
                .flat_map(IntoIterator::into_iter)
                .filter(|(_, r)| r.permissions.administrator())
                .map(|(&id, _)| id)
                .map(|role_id| role_id.mention().to_string())
                .collect::<Vec<String>>()
                .join(" ")
        }

        let mut response = String::new();

        if !self.succeeded.is_empty() {
            response.push_str("Successfully sent tokens to the following addresses:");
            for (addr, id) in self.succeeded.iter() {
                write!(
                    response,
                    "\n`{}` (try `pcli v tx {}`)",
                    addr.display_short_form(),
                    id
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
                "\n{mention_admins}: you may want to investigate this error :)",
                mention_admins = mention_admins(cache, guild_id).await,
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
