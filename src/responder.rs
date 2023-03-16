use penumbra_crypto::{Address, Value};
use penumbra_custody::CustodyClient;
use penumbra_transaction::Id;
use penumbra_view::ViewClient;
use serenity::prelude::TypeMapKey;
use tokio::sync::mpsc;
use tower::limit::ConcurrencyLimit;
use tower::Service;
use tower::ServiceExt;
use tracing::Instrument;

use crate::Sender;

mod request;
pub(crate) use request::AddressOrAlmost;
pub use request::Request;

mod response;
pub use response::Response;

/// Worker transforming lists of addresses to responses describing whether they were successfully
/// dispensed tokens.
pub struct Responder<V, C>
where
    V: ViewClient + Clone + Send + 'static,
    C: CustodyClient + Clone + Send + 'static,
{
    /// Maximum number of addresses to handle per message.
    max_addresses: usize,
    /// Actions to perform.
    actions: mpsc::Receiver<Request>,
    /// Values to send each time.
    values: Vec<Value>,
    /// The transaction sender.
    sender: ConcurrencyLimit<Sender<V, C>>,
}

/// `TypeMap` key for the address queue (so that `serenity` worker can send to it).
pub struct RequestQueue;

/// Associate the `AddressQueue` key with an `mpsc::Sender` for `AddressQueueMessage`s in the `TypeMap`.
impl TypeMapKey for RequestQueue {
    type Value = mpsc::Sender<Request>;
}

impl<V, C> Responder<V, C>
where
    V: ViewClient + Clone + Send + 'static,
    C: CustodyClient + Clone + Send + 'static,
{
    /// Create a new responder.
    pub fn new(
        sender: ConcurrencyLimit<Sender<V, C>>,
        max_addresses: usize,
        values: Vec<Value>,
    ) -> (mpsc::Sender<Request>, Self) {
        let (tx, rx) = mpsc::channel(10);
        (
            tx,
            Responder {
                sender,
                max_addresses,
                actions: rx,
                values,
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
        let mut succeeded = Vec::<(Address, Id)>::new();

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
                    let span = tracing::info_span!("send", address = %addr);
                    span.in_scope(|| {
                        tracing::info!("processing send request, waiting for readiness");
                    });
                    let rsp = self
                        .sender
                        .ready()
                        .await?
                        .call((*addr, self.values.clone()))
                        .instrument(span.clone());
                    tracing::info!("submitted send request");

                    match rsp.await {
                        Ok(id) => {
                            span.in_scope(|| {
                                tracing::info!(id = %id, "send request succeeded");
                            });
                            succeeded.push((*addr, id));
                        }
                        // By default, anyhow::Error's Display impl only prints the outermost error;
                        // using the alternate formate specifier prints the entire chain of causes.
                        Err(e) => failed.push((*addr, format!("{:#}", e))),
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
