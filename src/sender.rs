use std::{
    pin::Pin,
    task::{Context as TaskContext, Poll},
};

use futures::{Future, FutureExt};
use futures_util::Stream;
use penumbra_asset::Value;
use penumbra_custody::{AuthorizeRequest, CustodyClient};
use penumbra_keys::{Address, FullViewingKey};
use penumbra_transaction::memo::MemoPlaintext;
use penumbra_view::ViewClient;
use penumbra_wallet::plan::Planner;
use pin_project_lite::pin_project;
use rand::rngs::OsRng;
use tower::{discover::Change, limit::ConcurrencyLimit};
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
}

impl<V, C> Sender<V, C>
where
    V: ViewClient + Clone + Send + 'static,
    C: CustodyClient + Clone + Send + 'static,
{
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

impl<V, C> tower::Service<(Address, Vec<Value>)> for Sender<V, C>
where
    V: ViewClient + Clone + Send + 'static,
    C: CustodyClient + Clone + Send + 'static,
{
    type Response = penumbra_transaction::Id;
    type Error = anyhow::Error;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn call(&mut self, req: (Address, Vec<Value>)) -> Self::Future {
        let mut self2 = self.clone();
        async move {
            // 1. plan the transaction.
            let (address, values) = req;
            if values.is_empty() {
                return Err(anyhow::anyhow!(
                    "tried to send empty list of values to address"
                ));
            }
            let mut planner = Planner::new(OsRng);
            for value in values {
                planner.output(value, address);
            }
            planner
                .memo(MemoPlaintext {
                    text: "Hello from Galileo, the Penumbra faucet bot".to_string(),
                    sender: self2.fvk.payment_address(0.into()).0,
                })
                .unwrap();
            let plan = planner.plan(&mut self2.view, self2.fvk.wallet_id(), self2.account.into());
            let plan = plan.await?;

            // 2. Authorize and build the transaction.
            let auth_data = self2
                .custody
                .authorize(AuthorizeRequest {
                    plan: plan.clone(),
                    wallet_id: Some(self2.fvk.wallet_id()),
                    pre_authorizations: Vec::new(),
                })
                .await?
                .data
                .ok_or_else(|| anyhow::anyhow!("no auth data"))?
                .try_into()?;
            let witness_data = self2.view.witness(self2.fvk.wallet_id(), &plan).await?;
            let unauth_tx = plan
                .build_concurrent(OsRng, &self2.fvk, witness_data)
                .await?;

            let tx = unauth_tx.authorize(&mut OsRng, &auth_data)?;

            // 3. Broadcast the transaction and wait for confirmation.
            let (tx_id, _detection_height) = self2.view.broadcast_transaction(tx, true).await?;
            Ok(tx_id)
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
    S: Service<(Address, Vec<Value>), Response = penumbra_transaction::Id, Error = anyhow::Error>,
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
