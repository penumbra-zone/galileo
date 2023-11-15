use std::borrow::BorrowMut;
use std::collections::BTreeMap;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{bail, Context, Result};
use futures::stream::FuturesUnordered;
use futures_util::{Future, StreamExt as _};
use penumbra_asset::Value;
use penumbra_custody::CustodyClient;
use penumbra_keys::Address;
use penumbra_transaction::Id;
use penumbra_view::ViewClient;
use serenity::prelude::TypeMapKey;
use tokio::runtime::Handle;
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio::time::{sleep, Duration};
use tower::balance::p2c::Balance;
use tower::limit::ConcurrencyLimit;
use tower::load::PendingRequestsDiscover;
use tower::Service;
use tower::ServiceExt;
use tracing::{Instrument, Span};

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
    senders: Arc<
        Mutex<
            ConcurrencyLimit<
                Balance<
                    PendingRequestsDiscover<SenderSet<ConcurrencyLimit<Sender<V, C>>>>,
                    (Address, Vec<Value>),
                >,
            >,
        >,
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
                senders: Arc::new(Mutex::new(senders)),
                max_addresses,
                actions: rx,
                values,
            },
        )
    }

    /// Run the responder.
    pub async fn run(mut self, cancel_tx: tokio::sync::oneshot::Sender<()>) -> anyhow::Result<()> {
        while let Some(Request {
            addresses,
            response,
        }) = self.actions.recv().await
        {
            let reply = self.dispense(addresses).await?;
            let _ = response.send(reply.clone());
            sleep(Duration::from_millis(2000)).await;

            if !reply.failed().is_empty() {
                tracing::error!("failed to send funds to some addresses");
                cancel_tx.send(()).unwrap();
                return Err(anyhow::anyhow!("failed to send funds to some addresses"));
            }
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

        // Create a `FuturesUnordered` to track the send futures we're about to create and run.
        let mut tasks = Vec::new();
        // let mut tasks = FuturesUnordered::<
        //     Pin<
        //         Box<
        //             dyn Future<
        //                 Output = (
        //                     Address,
        //                     Result<
        //                         penumbra_transaction::Id,
        //                         std::boxed::Box<
        //                             dyn std::error::Error + std::marker::Send + std::marker::Sync,
        //                         >,
        //                     >,
        //                 ),
        //             >,
        //         >,
        //     >,
        // >::new();

        // Extract up to the maximum number of permissible valid addresses from the list
        let mut count = 0;
        while count <= self.max_addresses {
            count += 1;
            match addresses.pop() {
                Some(AddressOrAlmost::Address(addr)) => {
                    // Reply to the originating message with the address
                    let span = tracing::info_span!("send", address = %addr);
                    // TODO: could use tower "load" feature here
                    span.in_scope(|| {
                        tracing::info!("processing send request, waiting for readiness");
                    });

                    async fn send_request<Vi, Ci>(
                        addr: Address,
                        values: Vec<Value>,
                        span: Span,
                        senders: &mut ConcurrencyLimit<
                            Balance<
                                PendingRequestsDiscover<
                                    SenderSet<ConcurrencyLimit<Sender<Vi, Ci>>>,
                                >,
                                (Address, Vec<Value>),
                            >,
                        >,
                    ) -> (
                        Address,
                        Result<
                            penumbra_transaction::Id,
                            std::boxed::Box<
                                dyn std::error::Error + std::marker::Send + std::marker::Sync,
                            >,
                        >,
                    )
                    where
                        Vi: ViewClient + Clone + Send + 'static,
                        Ci: CustodyClient + Clone + Send + 'static,
                    {
                        (
                            addr,
                            senders
                                .call((addr, values.clone()))
                                .instrument(span.clone())
                                .await,
                        )
                    }

                    let values = self.values.clone();
                    let rsp = tokio::spawn(async {
                        let s = self.senders.clone().lock().await;
                        send_request(
                            *addr.clone(),
                            values,
                            span.clone(),
                            s.ready().await.unwrap(),
                        )
                    });

                    tracing::info!("submitted send request");

                    tasks.push(rsp);
                }
                Some(AddressOrAlmost::Almost(addr)) => {
                    unparsed.push(addr);
                }
                None => break,
            }
        }

        // Run the tasks concurrently.
        // while let Some((addr, result)) = tasks.next().await {
        for handle in tasks {
            let (addr, result) = handle.await.unwrap().await;
            match result {
                Ok(id) => {
                    // Reply to the originating message with the address
                    let span = tracing::info_span!("send", address = %addr);
                    span.in_scope(|| {
                        tracing::info!(id = %id, "send request succeeded");
                    });
                    succeeded.push((addr, id));
                }
                // By default, anyhow::Error's Display impl only prints the outermost error;
                // using the alternate formate specifier prints the entire chain of causes.
                Err(e) => {
                    tracing::error!(?addr, ?e, "Failed to send funds");
                    failed.push((addr, format!("{:#}", e)))
                }
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
