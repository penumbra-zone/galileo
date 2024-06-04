use anyhow::Context;
use penumbra_asset::Value;
use penumbra_custody::CustodyClient;
use penumbra_view::ViewClient;
use serenity::prelude::TypeMapKey;
use tokio::sync::mpsc;
use tower::{
    balance::p2c::Balance, buffer::Buffer, limit::ConcurrencyLimit, load::PendingRequestsDiscover,
};

use crate::sender::{Sender, SenderRequest, SenderSet};

mod dispense;

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
                SenderRequest,
            >,
        >,
        SenderRequest,
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
                SenderRequest,
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
                let reply =
                    self::dispense::dispense(addresses, self.max_addresses, values, senders)
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
