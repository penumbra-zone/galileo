use anyhow::Context;
use futures_util::stream;
use futures_util::StreamExt as _;
use penumbra_asset::Value;
use penumbra_custody::CustodyClient;
use penumbra_keys::Address;
use penumbra_txhash::TransactionId;
use penumbra_view::ViewClient;
use serenity::prelude::TypeMapKey;
use tokio::sync::mpsc;
use tower::balance::p2c::Balance;
use tower::buffer::Buffer;
use tower::limit::ConcurrencyLimit;
use tower::load::PendingRequestsDiscover;
use tower::ServiceExt;

use crate::sender::SenderSet;
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
    /// The transaction senders.
    senders: Buffer<
        ConcurrencyLimit<
            Balance<
                PendingRequestsDiscover<SenderSet<ConcurrencyLimit<Sender<V, C>>>>,
                (Address, Vec<Value>),
            >,
        >,
        (Address, Vec<Value>),
    >,
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
        senders: ConcurrencyLimit<
            Balance<
                PendingRequestsDiscover<SenderSet<ConcurrencyLimit<Sender<V, C>>>>,
                (Address, Vec<Value>),
            >,
        >,
        max_addresses: usize,
        values: Vec<Value>,
    ) -> (mpsc::Sender<Request>, Self) {
        let (tx, rx) = mpsc::channel(10);
        (
            tx,
            Responder {
                senders: Buffer::new(senders, 32),
                max_addresses,
                actions: rx,
                values,
            },
        )
    }

    /// Run the responder.
    pub async fn run(self, cancel_tx: tokio::sync::mpsc::Sender<()>) -> anyhow::Result<()> {
        let mut actions = self.actions;
        while let Some(Request {
            addresses,
            response,
        }) = actions.recv().await
        {
            let cancel_tx = cancel_tx.clone();
            let values = self.values.clone();
            let senders = self.senders.clone();
            tokio::spawn(async move {
                let reply = dispense(addresses, self.max_addresses, values, senders)
                    .await
                    .context("unable to dispense tokens")
                    .unwrap();

                let _ = response.send(reply.clone());

                if !reply.failed().is_empty() {
                    tracing::error!("failed to send funds to some addresses");
                    cancel_tx.send(()).await.expect("able to send cancellation");
                }
            });
        }

        Ok(())
    }
}

/// Try to dispense tokens to the given addresses, collecting [`Response`] describing what
/// happened.
async fn dispense<V, C>(
    mut addresses: Vec<AddressOrAlmost>,
    max_addresses: usize,
    values: Vec<Value>,
    senders: Buffer<
        ConcurrencyLimit<
            Balance<
                PendingRequestsDiscover<SenderSet<ConcurrencyLimit<Sender<V, C>>>>,
                (Address, Vec<Value>),
            >,
        >,
        (Address, Vec<Value>),
    >,
) -> anyhow::Result<Response>
where
    V: ViewClient + Clone + Send + 'static,
    C: CustodyClient + Clone + Send + 'static,
{
    // Track addresses to which we successfully dispensed tokens
    let mut succeeded = Vec::<(Address, TransactionId)>::new();

    // Track addresses (and associated errors) which we tried to send tokens to, but failed
    let mut failed = Vec::<(Address, String)>::new();

    // Track addresses which couldn't be parsed
    let mut unparsed = Vec::<String>::new();

    // Track the requests to send to the sender service.
    let mut reqs = Vec::new();

    // Extract up to the maximum number of permissible valid addresses from the list
    let mut sent_addresses = Vec::new();
    let mut count = 0;
    while count <= max_addresses {
        count += 1;
        match addresses.pop() {
            Some(AddressOrAlmost::Address(addr)) => {
                // Reply to the originating message with the address
                let span = tracing::info_span!("send", address = %addr);
                // TODO: could use tower "load" feature here
                span.in_scope(|| {
                    tracing::info!("processing send request, waiting for readiness");
                });

                let req = (*addr.clone(), values.clone());
                reqs.push(req);
                sent_addresses.push(addr.clone());

                span.in_scope(|| {
                    tracing::info!("submitted send request");
                });
            }
            Some(AddressOrAlmost::Almost(addr)) => {
                unparsed.push(addr);
            }
            None => break,
        }
    }

    let reqs = stream::iter(reqs);

    // Will return in ordered fashion, so we know the address in the `called` list
    // corresponds to the task handle at the same index.
    let mut responses = senders.call_all(reqs);

    // Run the tasks concurrently.
    let mut i = 0;
    while let Some(result) = responses.next().await {
        let addr = &sent_addresses[i];
        match result {
            Ok(id) => {
                // Reply to the originating message with the address
                let span = tracing::info_span!("send", address = %addr);
                span.in_scope(|| {
                    tracing::info!(id = %id, "send request succeeded");
                });
                succeeded.push((*addr.clone(), id));
            }
            // By default, anyhow::Error's Display impl only prints the outermost error;
            // using the alternate formate specifier prints the entire chain of causes.
            Err(e) => {
                tracing::error!(?addr, ?e, "Failed to send funds");
                failed.push((*addr.clone(), format!("{:#}", e)))
            }
        }

        i += 1;
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
