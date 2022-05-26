use std::time::Duration;

use penumbra_crypto::{Address, Value};
use serenity::prelude::TypeMapKey;
use tokio::{sync::mpsc, time::sleep};

use crate::wallet;

mod request;
pub(crate) use request::AddressOrAlmost;
pub use request::Request;

mod response;
pub use response::Response;

/// Worker transforming lists of addresses to responses describing whether they were successfully
/// dispensed tokens.
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

/// `TypeMap` key for the address queue (so that `serenity` worker can send to it).
pub struct RequestQueue;

/// Associate the `AddressQueue` key with an `mpsc::Sender` for `AddressQueueMessage`s in the `TypeMap`.
impl TypeMapKey for RequestQueue {
    type Value = mpsc::Sender<Request>;
}

impl Responder {
    /// Create a new responder.
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

    /// Run the responder.
    pub async fn run(mut self) -> anyhow::Result<()> {
        while let Some(Request {
            addresses,
            response,
        }) = self.actions.recv().await
        {
            let reply = self.dispense(addresses).await?;
            let _ = response.send(reply);
        }

        Ok(())
    }

    /// Try to dispense tokens to the given addresses, collecting [`Response`] describing what
    /// happened.
    async fn dispense(&mut self, mut addresses: Vec<AddressOrAlmost>) -> anyhow::Result<Response> {
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
                    tracing::info!(address = %addr, "requesting tokens");

                    let (result, request) =
                        wallet::Request::send(*addr, self.values.clone(), self.fee);
                    self.requests.send(request).await?;

                    // temp: Remove after wallet refactor. This is to prevent reuse of spent notes.
                    sleep(Duration::from_secs(10)).await;

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
        let mut remaining = Vec::<Address>::new();
        for addr in addresses {
            match addr {
                AddressOrAlmost::Address(addr) => remaining.push(*addr),
                AddressOrAlmost::Almost(addr) => unparsed.push(addr),
            }
        }

        Ok(Response {
            succeeded,
            failed,
            unparsed,
            remaining,
        })
    }
}
