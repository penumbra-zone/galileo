use derivative::Derivative;
use penumbra_crypto::{keys::SpendKey, transaction::Fee, Address, FullViewingKey, Value};
use penumbra_custody::CustodyClient;
use penumbra_transaction::Transaction;
use penumbra_view::ViewClient;
use penumbra_wallet::{build_transaction, plan};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
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
    source: Option<u64>,
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
        serde_json::from_slice(std::fs::read(path)?.as_slice()).map_err(Into::into)
    }
}

impl<V: ViewClient, C: CustodyClient> WalletWorker<V, C> {
    /// Create a new wallet worker.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        view: V,
        custody: C,
        fvk: FullViewingKey,
        source_address: Option<u64>,
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
    #[instrument(skip(self, destination, amounts), fields(%destination))]
    async fn dispense(
        &mut self,
        destination: Address,
        amounts: Vec<Value>,
        fee: u64,
    ) -> anyhow::Result<()> {
        tracing::debug!("Dispensing...");

        let source_address = self.source;

        let plan = plan::send(
            &self.fvk,
            &mut self.view,
            OsRng,
            &amounts,
            Fee::from_staking_token_amount(fee.into()),
            destination,
            source_address,
            None,
        )
        .await?;
        tracing::debug!("plan: {:#?}", plan);

        let transaction =
            build_transaction(&self.fvk, &mut self.view, &mut self.custody, OsRng, plan).await?;
        tracing::debug!("transaction: {:#?}", transaction);

        // TODO: extract into penumbra_wallet crate?
        self.submit_transaction(&transaction).await?;

        Ok(())
    }

    /// Submits a transaction to the network, returning `Ok` only when the remote
    /// node has accepted the transaction, and erroring otherwise.
    #[instrument(skip(self, transaction))]
    pub async fn submit_transaction(&self, transaction: &Transaction) -> Result<(), anyhow::Error> {
        use penumbra_proto::DomainType;
        tracing::info!("broadcasting transaction...");

        let client = reqwest::Client::new();
        let req_id: u8 = rand::random();
        let rsp: serde_json::Value = client
            .post(format!(r#"http://{}:{}"#, self.node, self.rpc_port))
            .json(&serde_json::json!(
                {
                    "method": "broadcast_tx_sync",
                    "params": [&transaction.encode_to_vec()],
                    "id": req_id,
                }
            ))
            .send()
            .await?
            .json()
            .await?;

        tracing::info!("{}", rsp);

        // Sometimes the result is in a result key, and sometimes it's bare? (??)
        let result = rsp.get("result").unwrap_or(&rsp);

        let code = result
            .get("code")
            .and_then(|c| c.as_i64())
            .ok_or_else(|| anyhow::anyhow!("could not parse JSON response"))?;

        if code == 0 {
            Ok(())
        } else {
            let log = result
                .get("log")
                .and_then(|l| l.as_str())
                .ok_or_else(|| anyhow::anyhow!("could not parse JSON response"))?;

            Err(anyhow::anyhow!(
                "Error submitting transaction: code {}, log: {}",
                code,
                log
            ))
        }
    }
}
