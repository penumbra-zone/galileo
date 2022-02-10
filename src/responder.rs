use std::{borrow::Borrow, pin::Pin};

use derivative::Derivative;
use futures::Future;
use penumbra_crypto::{Address, Value};
use regex::Regex;
use serenity::{model::channel::Message, prelude::TypeMapKey};
use tokio::sync::{mpsc, oneshot};

use crate::wallet;

pub struct Responder {
    /// Maximum number of addresses to handle per message.
    max_addresses: usize,
    /// Actions to perform.
    actions: mpsc::Receiver<Request>,
    /// Requests outbound to the wallet worker.
    requests: mpsc::Sender<wallet::Request>,
    /// Values to send each time.
    values: Vec<Value>,
    /// Fee to send each time.
    fee: u64,
}

/// `TypeMap` key for the address queue.
pub struct RequestQueue;

/// Associate the `AddressQueue` key with an `mpsc::Sender` for `AddressQueueMessage`s in the `TypeMap`.
impl TypeMapKey for RequestQueue {
    type Value = mpsc::Sender<Request>;
}

/// A request to be fulfilled by the responder service.
#[derive(Derivative)]
#[derivative(Debug)]
pub struct Request {
    /// The originating message that contained these addresses.
    message: Message,
    /// The addresses matched in the originating message.
    addresses: Vec<AddressOrAlmost>,
    /// The sender for the response.
    response: oneshot::Sender<(Message, String)>,
    /// A future which can be invoked to obtain a mention-string for the admins of the server.
    #[derivative(Debug = "ignore")]
    mention_admins: Pin<Box<dyn Future<Output = String> + Send + Sync + 'static>>,
}

impl Request {
    pub fn message(&self) -> &Message {
        &self.message
    }

    pub fn try_new(
        message: Message,
        mention_admins: impl Future<Output = String> + Send + Sync + 'static,
    ) -> Result<(oneshot::Receiver<(Message, String)>, Request), Message> {
        let address_regex =
            Regex::new(r"penumbrav\dt1[qpzry9x8gf2tvdw0s3jn54khce6mua7l]{126}").unwrap();

        // Collect all the matches into a struct, bundled with the original message
        tracing::trace!("collecting addresses from message");
        let addresses: Vec<AddressOrAlmost> = address_regex
            .find_iter(&message.content)
            .map(|m| {
                use AddressOrAlmost::*;
                match m.as_str().parse() {
                    Ok(addr) => Address(Box::new(addr)),
                    Err(e) => {
                        tracing::trace!(error = ?e, "failed to parse address");
                        Almost(m.as_str().to_string())
                    }
                }
            })
            .collect();

        // If no addresses were found, don't bother sending the message to the queue
        if addresses.is_empty() {
            Err(message)
        } else {
            let (tx, rx) = oneshot::channel();
            Ok((
                rx,
                Request {
                    message,
                    addresses,
                    response: tx,
                    mention_admins: Box::pin(mention_admins),
                },
            ))
        }
    }
}

#[derive(Debug, Clone)]
pub enum AddressOrAlmost {
    Address(Box<Address>),
    Almost(String),
}

impl Responder {
    pub fn new(
        requests: mpsc::Sender<wallet::Request>,
        max_addresses: usize,
        buffer_size: usize,
        values: Vec<Value>,
        fee: u64,
    ) -> (mpsc::Sender<Request>, Self) {
        let (tx, rx) = mpsc::channel(buffer_size);
        (
            tx,
            Responder {
                requests,
                max_addresses,
                actions: rx,
                values,
                fee,
            },
        )
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        while let Some(Request {
            addresses,
            message,
            response,
            mention_admins,
        }) = self.actions.recv().await
        {
            let reply = self.dispense(&message, addresses, mention_admins).await?;
            let _ = response.send((message, reply));
        }

        Ok(())
    }

    async fn dispense(
        &mut self,
        message: impl Borrow<Message>,
        mut addresses: Vec<AddressOrAlmost>,
        mention_admins: impl Future<Output = String> + Send + Sync + 'static,
    ) -> anyhow::Result<String> {
        let message = message.borrow();

        // Track addresses to which we successfully dispensed tokens
        let mut succeeded = Vec::<(Address, Vec<Value>)>::new();

        // Track addresses (and associated errors) which we tried to send tokens to, but failed
        let mut failed = Vec::<(Address, String)>::new();

        // Track addresses which couldn't be parsed
        let mut unparsed = Vec::<String>::new();

        // Extract up to the maximum number of permissible valid addresses from the list
        let mut count = 0;
        while count <= self.max_addresses {
            count += 1;
            match addresses.pop() {
                Some(AddressOrAlmost::Address(addr)) => {
                    // Reply to the originating message with the address
                    tracing::info!(
                        user_name = ?message.author.name,
                        user_id = ?message.author.id.to_string(),
                        address = %addr,
                        "requesting tokens"
                    );

                    let (result, request) =
                        wallet::Request::send(*addr, self.values.clone(), self.fee);
                    self.requests.send(request).await?;

                    match result.await? {
                        Ok(()) => succeeded.push((*addr, self.values.clone())),
                        Err(e) => failed.push((*addr, e.to_string())),
                    }
                }
                Some(AddressOrAlmost::Almost(addr)) => {
                    unparsed.push(addr);
                }
                None => break,
            }
        }

        // Separate the rest of the list into unparsed and remaining valid ones
        let mut remaining_addresses = Vec::<Address>::new();
        for addr in addresses {
            match addr {
                AddressOrAlmost::Address(addr) => remaining_addresses.push(*addr),
                AddressOrAlmost::Almost(addr) => unparsed.push(addr),
            }
        }

        // Reply with a summary of what occurred
        let response = self
            .dispense_summary(
                &succeeded,
                &failed,
                &unparsed,
                &remaining_addresses,
                mention_admins,
            )
            .await;

        Ok(response)
    }

    async fn dispense_summary(
        &mut self,
        succeeded: &[(Address, Vec<Value>)],
        failed: &[(Address, String)],
        unparsed: &[String],
        remaining: &[Address],
        mention_admins: impl Future<Output = String> + Send + 'static,
    ) -> String {
        let succeeded_addresses = succeeded.borrow();
        let failed_addresses = failed.borrow();
        let remaining_addresses = remaining.borrow();

        let mut response = String::new();

        if !succeeded_addresses.is_empty() {
            response.push_str("Successfully sent tokens to the following addresses:");
            for (addr, _values) in succeeded_addresses {
                response.push_str(&format!("\n`{}`", addr));
            }
        }

        if !failed_addresses.is_empty() {
            response.push_str("Failed to send tokens to the following addresses:");
            for (addr, error) in failed_addresses {
                response.push_str(&format!("\n`{}` (error: {})", addr, error));
            }

            response.push_str(&format!(
                "\n{mention_admins}: you may want to investigate this error :)",
                mention_admins = mention_admins.await,
            ))
        }

        if !unparsed.is_empty() {
            response.push_str(
                "\nThe following _look like_ Penumbra addresses, \
                but are invalid (maybe a typo or old address version?):",
            );
            for addr in unparsed {
                response.push_str(&format!("\n`{}`", addr));
            }
        }

        if !remaining_addresses.is_empty() {
            response.push_str(&format!(
                "\nI'm only allowed to send tokens to addresses {} at a time; \
                try again later to get tokens for the following addresses:",
                self.max_addresses,
            ));
            for addr in remaining_addresses {
                response.push_str(&format!("\n`{}`", addr));
            }
        }

        response
    }
}
