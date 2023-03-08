use anyhow::Context;
use derivative::Derivative;
use penumbra_crypto::{keys::SpendKey, Address, FullViewingKey, Value};
use penumbra_custody::{AuthorizeRequest, CustodyClient};
use penumbra_view::ViewClient;
use penumbra_wallet::plan::Planner;
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use std::str::FromStr;
use tokio::sync::{mpsc, oneshot};
use tracing::instrument;

/// The wallet worker, responsible for periodically synchronizing blocks from the chain and
/// transmitting transactions to the network.
#[derive(Derivative)]
#[derivative(Debug)]
pub struct WalletWorker<V: ViewClient, C: CustodyClient> {
    #[derivative(Debug = "ignore")]
    view: V,
    #[derivative(Debug = "ignore")]
    custody: C,
    fvk: FullViewingKey,
    requests: Option<mpsc::Receiver<Request>>,
    node: String,
    rpc_port: u16,
    source: penumbra_crypto::keys::AddressIndex,
}

/// A request that the wallet dispense the listed tokens to a particular address, using some fee.
#[derive(Debug)]
pub struct Request {
    destination: Address,
    amounts: Vec<Value>,
    fee: u64,
    result: oneshot::Sender<Result<(), anyhow::Error>>,
}

impl Request {
    pub fn send(
        destination: Address,
        amounts: Vec<Value>,
        fee: u64,
    ) -> (oneshot::Receiver<anyhow::Result<()>>, Self) {
        let (tx, rx) = oneshot::channel();
        let request = Request {
            destination,
            amounts,
            result: tx,
            fee,
        };
        (rx, request)
    }
}

/// A wallet file storing a single spend authority.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Wallet {
    pub spend_key: SpendKey,
}

impl Wallet {
    /// Read the wallet data from the provided path.
    pub fn load(path: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        let custody_json: serde_json::Value =
            serde_json::from_slice(std::fs::read(path)?.as_slice())?;
        let sk_str = match custody_json["spend_key"].as_str() {
            Some(s) => s,
            None => {
                return Err(anyhow::anyhow!(
                    "'spend_key' field not found in custody JSON file"
                ))
            }
        };
        let spend_key = SpendKey::from_str(sk_str)
            .context(format!("Could not create SpendKey from string: {}", sk_str))?;
        Ok(Self { spend_key })
    }
}

impl<V: ViewClient, C: CustodyClient> WalletWorker<V, C> {
    /// Create a new wallet worker.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        view: V,
        custody: C,
        fvk: FullViewingKey,
        source_address: penumbra_crypto::keys::AddressIndex,
        node: String,
        rpc_port: u16,
    ) -> (mpsc::Sender<Request>, Self) {
        let (tx, rx) = mpsc::channel(10);

        (
            tx,
            Self {
                view,
                custody,
                fvk,
                requests: Some(rx),
                node,
                rpc_port,
                source: source_address,
            },
        )
    }

    /// Run the wallet worker.
    pub async fn run(mut self) -> anyhow::Result<()> {
        let mut requests = self.requests.take().unwrap();
        while let Some(Request {
            destination,
            amounts,
            result,
            fee,
        }) = requests.recv().await
        {
            tracing::debug!("sending back result of request");
            let _ = result.send(self.dispense(destination, amounts, fee).await);
        }
        tracing::trace!("wallet request senders all dropped");
        Ok(())
    }

    /// Construct a transaction sending the given values to the given address, with the given fee,
    /// and transmit it to the network, waiting for it to be accepted.
    #[instrument(skip(self, destination, values), fields(%destination))]
    async fn dispense(
        &mut self,
        destination: Address,
        values: Vec<Value>,
        fee: u64,
    ) -> anyhow::Result<()> {
        tracing::debug!("Dispensing...");

        let source_address = self.source;

        // 1. plan the transaction.
        if values.is_empty() {
            return Err(anyhow::anyhow!(
                "tried to send empty list of values to address"
            ));
        }
        let mut planner = Planner::new(OsRng);
        for value in values {
            planner.output(value, destination);
        }
        // TODO: look up galileo bot's wallet address and include in memo; required since
        // https://github.com/penumbra-zone/penumbra/issues/1880
        // planner
        //     .memo("Hello from Galileo, the Penumbra faucet bot".to_string())
        //     .unwrap();
        let plan = planner.plan(&mut self.view, &self.fvk, source_address);
        let plan = plan.await?;

        // 2. Authorize and build the transaction.
        let auth_data = self
            .custody
            .authorize(AuthorizeRequest {
                plan: plan.clone(),
                account_id: self.fvk.hash(),
                pre_authorizations: Vec::new(),
            })
            .await?
            .data
            .ok_or_else(|| anyhow::anyhow!("no auth data"))?
            .try_into()?;
        let witness_data = self.view.witness(self.fvk.hash(), &plan).await?;
        let tx = plan
            .build_concurrent(OsRng, &self.fvk, auth_data, witness_data)
            .await?;

        // 3. Broadcast the transaction and wait for confirmation.
        self.view.broadcast_transaction(tx, true).await?;
        Ok(())
    }
}
