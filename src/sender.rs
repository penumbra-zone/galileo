use std::{pin::Pin, task::Poll};

use futures::{Future, FutureExt};
use penumbra_crypto::{memo::MemoPlaintext, Address, FullViewingKey, Value};
use penumbra_custody::{AuthorizeRequest, CustodyClient};
use penumbra_view::ViewClient;
use penumbra_wallet::plan::Planner;
use rand::rngs::OsRng;
use tower::limit::ConcurrencyLimit;

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
            let plan = planner.plan(
                &mut self2.view,
                self2.fvk.account_group_id(),
                self2.account.into(),
            );
            let plan = plan.await?;

            // 2. Authorize and build the transaction.
            let auth_data = self2
                .custody
                .authorize(AuthorizeRequest {
                    plan: plan.clone(),
                    account_group_id: self2.fvk.account_group_id(),
                    pre_authorizations: Vec::new(),
                })
                .await?
                .data
                .ok_or_else(|| anyhow::anyhow!("no auth data"))?
                .try_into()?;
            let witness_data = self2
                .view
                .witness(self2.fvk.account_group_id(), &plan)
                .await?;
            let tx = plan
                .build_concurrent(OsRng, &self2.fvk, auth_data, witness_data)
                .await?;

            // 3. Broadcast the transaction and wait for confirmation.
            self2.view.broadcast_transaction(tx, true).await
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
