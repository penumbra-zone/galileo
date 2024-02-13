use std::{
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context as TaskContext, Poll},
    time::Duration,
};

use futures::{Future, FutureExt};
use futures_util::Stream;
use futures_util::TryStreamExt;

use penumbra_proto::view::v1::broadcast_transaction_response::Status as BroadcastStatus;

use penumbra_asset::Value;
use penumbra_custody::{AuthorizeRequest, CustodyClient};
use penumbra_keys::{Address, FullViewingKey};
use penumbra_transaction::memo::MemoPlaintext;
use penumbra_txhash::TransactionId;
use penumbra_view::ViewClient;
use penumbra_wallet::plan::Planner;
use pin_project_lite::pin_project;
use rand::rngs::OsRng;
use tokio::sync::broadcast;
use tower::discover::Change;
use tower_batch::{Batch, BatchControl};
use tower_service::Service;

/// The `Sender` maps `(Address, Vec<Value>)` send requests to `[u8; 32]` transaction hashes of sent funds.
#[derive(Clone)]
pub struct Sender<V, C>
where
    V: ViewClient + Clone + 'static,
    C: CustodyClient + Clone + 'static,
{
    view: V,
    custody: C,
    fvk: FullViewingKey,
    account: u32,
    // Accumulate a batch of transactions to perform
    #[allow(clippy::type_complexity)]
    batch: Arc<Mutex<Vec<(Address, Vec<Value>)>>>,
    // Broadcast the transaction hash to everyone who requested in the batch
    result: broadcast::Sender<TransactionId>,
}

impl<V, C> Sender<V, C>
where
    V: ViewClient + Clone + Send + 'static,
    C: CustodyClient + Clone + Send + 'static,
{
    pub fn new(
        account: u32,
        fvk: FullViewingKey,
        view: V,
        custody: C,
    ) -> Batch<Self, (Address, Vec<Value>)> {
        Batch::new(
            tower::ServiceBuilder::new().service(Self {
                view,
                custody,
                fvk,
                account,
                batch: Arc::new(Mutex::new(Vec::new())),
                result: broadcast::channel(1).0,
            }),
            25,
            Duration::from_secs(5),
        )
    }
}

impl<V, C> tower::Service<BatchControl<(Address, Vec<Value>)>> for Sender<V, C>
where
    V: ViewClient + Clone + Send + 'static,
    C: CustodyClient + Clone + Send + 'static,
{
    type Response = TransactionId;
    type Error = anyhow::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn call(&mut self, req: BatchControl<(Address, Vec<Value>)>) -> Self::Future {
        let mut self2 = self.clone();
        async move {
            if let BatchControl::Item((address, values)) = req {
                self2.batch.lock().unwrap().push((address, values));
                Ok(self2.result.subscribe().recv().await?)
            } else {
                // Process the batch when the control is `Flush`.
                let requests = std::mem::take(&mut *self2.batch.lock().unwrap());

                // 1. plan the transaction.
                if requests.is_empty() {
                    return Err(anyhow::anyhow!(
                        "tried to send empty list of values to address"
                    ));
                }

                let mut planner = Planner::new(OsRng);
                for (address, values) in requests {
                    for value in values {
                        planner.output(value, address);
                    }
                }
                planner
                    .memo(
                        MemoPlaintext::new(
                            self2.fvk.payment_address(0.into()).0,
                            "Hello from Galileo, the Penumbra faucet bot".to_string(),
                        )
                        .expect("can create memo"),
                    )
                    .unwrap();
                let plan = planner.plan(&mut self2.view, self2.account.into()).await?;

                // 2. Authorize and build the transaction.
                let auth_data = self2
                    .custody
                    .authorize(AuthorizeRequest {
                        plan: plan.clone(),
                        pre_authorizations: Vec::new(),
                    })
                    .await?
                    .data
                    .ok_or_else(|| anyhow::anyhow!("no auth data"))?
                    .try_into()?;
                let witness_data = self2.view.witness(&plan).await?;
                let tx = plan
                    .build_concurrent(&self2.fvk, &witness_data, &auth_data)
                    .await?;

                // 3. Broadcast the transaction and wait for confirmation.
                let id = tx.id();
                let mut rsp = self2.view.broadcast_transaction(tx, true).await?;
                while let Some(rsp) = rsp.try_next().await? {
                    match rsp.status {
                        Some(BroadcastStatus::BroadcastSuccess(_)) => {
                            println!("transaction broadcast successfully: {}", id);
                        }
                        Some(BroadcastStatus::Confirmed(c)) => {
                            println!(
                                "transaction confirmed and detected: {} @ height {}",
                                id, c.detection_height
                            );
                            let _ = self2.result.send(id);
                            return Ok(self2.result.subscribe().recv().await?);
                        }
                        _ => {}
                    }
                }

                Err(anyhow::anyhow!(
                    "view server closed stream without reporting transaction confirmation"
                ))
            }
        }
        .boxed()
    }

    fn poll_ready(
        &mut self,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        // We always return "ready" because we handle backpressure by making the
        // constructor return a concurrency limit wrapper.
        Poll::Ready(Ok(()))
    }
}

type Key = u32;

pin_project! {
pub struct SenderSet<S> {
    services: Vec<(Key, S)>,
}
}

impl<S> SenderSet<S> {
    pub fn new(services: Vec<(Key, S)>) -> Self {
        Self { services }
    }
}

impl<S> Stream for SenderSet<S>
where
    S: Service<(Address, Vec<Value>), Response = TransactionId>,
{
    type Item = Result<Change<Key, S>, S::Error>;

    fn poll_next(self: Pin<&mut Self>, _: &mut TaskContext<'_>) -> Poll<Option<Self::Item>> {
        match self.project().services.pop() {
            Some((k, service)) => Poll::Ready(Some(Ok(Change::Insert(k, service)))),
            None => {
                // there may be more later
                Poll::Pending
            }
        }
    }
}
