use std::{
    pin::Pin,
    task::{Context as TaskContext, Poll},
};

use futures::{Future, FutureExt};
use futures_util::Stream;
use futures_util::TryStreamExt;
use penumbra_asset::Value;
use penumbra_custody::{AuthorizeRequest, CustodyClient};
use penumbra_fee::FeeTier;
use penumbra_fee::GasPrices;
use penumbra_keys::{Address, FullViewingKey};
use penumbra_proto::view::v1::broadcast_transaction_response::Status as BroadcastStatus;
use penumbra_txhash::TransactionId;
use penumbra_view::ViewClient;
use penumbra_wallet::plan::Planner;
use pin_project_lite::pin_project;
use rand::rngs::OsRng;
use tower::{discover::Change, limit::ConcurrencyLimit};
use tower_service::Service;

pub mod service;

/// A send request is an [`Address`], and a collection of [`Value`]s to send it.
pub type SenderRequest = (Address, Vec<Value>);

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
}

pin_project! {
    /// A set of [`Sender`]s.
    pub struct SenderSet<S> {
        services: Vec<(Key, S)>,
    }
}

/// A key for a sender in a [`SenderSet`].
type Key = u32;

// === impl Sender ===

impl<V, C> Sender<V, C>
where
    V: ViewClient + Clone + Send + 'static,
    C: CustodyClient + Clone + Send + 'static,
{
    /// Constructs a new sender.
    ///
    /// The value returned by this function is implicitly wrapped in a concurrency limiter, so that
    /// this sender may only handle one request at a time.
    pub fn new(account: u32, fvk: FullViewingKey, view: V, custody: C) -> ConcurrencyLimit<Self> {
        tower::ServiceBuilder::new()
            .concurrency_limit(1)
            .service(Self {
                view,
                custody,
                fvk,
                account,
            })
    }
}

impl<V, C> tower::Service<SenderRequest> for Sender<V, C>
where
    V: ViewClient + Clone + Send + 'static,
    C: CustodyClient + Clone + Send + 'static,
{
    type Response = TransactionId;
    type Error = anyhow::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn call(&mut self, (address, values): SenderRequest) -> Self::Future {
        let mut self2 = self.clone();
        async move {
            // 1. plan the transaction.
            if values.is_empty() {
                return Err(anyhow::anyhow!(
                    "tried to send empty list of values to address"
                ));
            }

            let mut planner = Planner::new(OsRng);

            // Here we hardcode low fees. It'd be nice to override via CLI arg.
            // Look up GasPrices, because merely calling `set_fee_tier` is not sufficient.
            let gp: GasPrices = self2
                .view
                .gas_prices()
                .await
                .expect("failed to look up GasPrices");
            planner.set_gas_prices(gp);
            planner.set_fee_tier(FeeTier::Low);

            for value in values {
                planner.output(value, address.clone());
            }
            planner.memo("Hello from Galileo, the Penumbra faucet bot".to_string());
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
                        return Ok(id);
                    }
                    _ => {}
                }
            }
            Err(anyhow::anyhow!(
                "view server closed stream without reporting transaction confirmation"
            ))
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

// === impl SenderSet ===

impl<S> SenderSet<S> {
    /// Constructs a new sender set.
    pub fn new<I>(services: I) -> Self
    where
        I: IntoIterator<Item = (Key, S)>,
    {
        Self {
            services: services.into_iter().collect(),
        }
    }
}

impl<S> Stream for SenderSet<S>
where
    S: Service<SenderRequest, Response = TransactionId, Error = anyhow::Error>,
{
    type Item = Result<Change<Key, S>, anyhow::Error>;

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
