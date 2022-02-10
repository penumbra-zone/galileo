use std::{path::PathBuf, time::Duration};

use anyhow::Context;
use penumbra_crypto::{asset, Address, Value};
use penumbra_proto::{
    light_wallet::{
        light_wallet_client::LightWalletClient, ChainParamsRequest, CompactBlockRangeRequest,
    },
    thin_wallet::{thin_wallet_client::ThinWalletClient, AssetListRequest},
};
use penumbra_transaction::Transaction;
use penumbra_wallet::ClientState;
use rand::rngs::OsRng;
use tokio::{
    io::AsyncWriteExt,
    sync::{mpsc, oneshot, watch},
    time::Instant,
};
use tonic::transport::Channel;
use tracing::instrument;

#[derive(Debug)]
pub struct Wallet {
    client_state: Option<ClientState>,
    client_state_path: PathBuf,
    sync_interval: Duration,
    save_interval: Duration,
    requests: mpsc::Receiver<Request>,
    initial_sync: watch::Sender<bool>,
    node: String,
    light_wallet_port: u16,
    thin_wallet_port: u16,
    rpc_port: u16,
    source: Option<u64>,
    last_saved: Option<Instant>,
}

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

impl Wallet {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        client_state_path: PathBuf,
        source_address: Option<u64>,
        sync_interval: Duration,
        save_interval: Duration,
        buffer_size: usize,
        node: String,
        light_wallet_port: u16,
        thin_wallet_port: u16,
        rpc_port: u16,
    ) -> (mpsc::Sender<Request>, watch::Receiver<bool>, Self) {
        let (tx, rx) = mpsc::channel(buffer_size);
        let (watch_tx, watch_rx) = watch::channel(false);
        (
            tx,
            watch_rx,
            Self {
                client_state: None,
                client_state_path,
                sync_interval,
                save_interval,
                requests: rx,
                initial_sync: watch_tx,
                node,
                light_wallet_port,
                thin_wallet_port,
                rpc_port,
                source: source_address,
                last_saved: None,
            },
        )
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        let wallet_lock = self.lock_wallet().await?; // lock wallet file while running
        let mut sync_duration = None;
        loop {
            tokio::select! {
                _ = tokio::time::sleep(sync_duration.unwrap_or(Duration::ZERO)) => {
                    tracing::trace!("syncing wallet");
                    self.sync().await?;
                    if *self.initial_sync.borrow() {
                        sync_duration = Some(self.sync_interval);
                    }
                },
                request = self.requests.recv() => match request {
                    Some(Request { destination, amounts, result, fee }) => if *self.initial_sync.borrow() {
                        tracing::trace!("sending back result of request");
                        let _ = result.send(self.dispense(destination, amounts, fee).await);
                    } else {
                        tracing::trace!("waiting for initial sync");
                        let _ = result.send(Err(anyhow::anyhow!("still performing initial sync, please wait")));
                    }
                    None => {
                        tracing::trace!("wallet request senders all dropped");
                        break;
                    }
                },
            }
        }
        drop(wallet_lock);
        Ok(())
    }

    /// Syncs blocks until the sync interval times out, then writes the state to disk.
    #[instrument(skip(self))]
    async fn sync(&mut self) -> anyhow::Result<()> {
        if !*self.initial_sync.borrow() {
            tracing::info!(
                "starting initial sync: please wait for sync to complete before requesting tokens"
            );
        }

        let sync_interval = self.sync_interval;

        let mut light_client = self.light_wallet_client().await?;

        // If the wallet does not yet have chain parameters set, update them
        self.fetch_chain_params().await?;

        // Update blocks
        let start = Instant::now();
        tracing::trace!("starting sync");

        let start_height = self
            .state()
            .await?
            .last_block_height()
            .map(|h| h + 1)
            .unwrap_or(0);
        let mut stream = light_client
            .compact_block_range(tonic::Request::new(CompactBlockRangeRequest {
                start_height,
                end_height: 0,
                chain_id: self
                    .state()
                    .await?
                    .chain_id()
                    .ok_or_else(|| anyhow::anyhow!("missing chain_id"))?,
            }))
            .await?
            .into_inner();

        let mut count = 0;
        while start.elapsed() < sync_interval {
            if let Some(block) = stream.message().await? {
                if block.height % 1000 == 0 {
                    tracing::debug!(height = ?block.height, ?count, "scanning block");
                }
                self.state().await?.scan_block(block)?;
                self.save_state().await?;
                count += 1;
            } else {
                // Get the height at which we've finished syncing the chain
                let height = self.state().await?.last_block_height().unwrap_or(0);

                // Update asset registry after finishing sync to the top of the chain
                self.update_asset_registry().await?;

                if !*self.initial_sync.borrow() {
                    tracing::info!(?height, "initial sync complete: ready to process requests");
                } else {
                    tracing::debug!(?height, ?count, "finished sync");
                }
                let _ = self.initial_sync.send(true);
                return Ok(());
            }
        }

        let height = self.state().await?.last_block_height().unwrap_or(0);
        tracing::info!(?height, ?count, "syncing...");

        Ok(())
    }

    #[instrument(skip(self))]
    async fn dispense(
        &mut self,
        destination: Address,
        amounts: Vec<Value>,
        fee: u64,
    ) -> anyhow::Result<()> {
        let source_address = self.source;
        let state = self.state().await?;
        let transaction =
            state.build_send(&mut OsRng, &amounts, fee, destination, source_address, None)?;
        self.submit_transaction(&transaction).await?;
        self.save_state().await?;
        Ok(())
    }

    async fn state(&mut self) -> anyhow::Result<&mut ClientState> {
        if self.client_state.is_none() {
            tracing::trace!(?self.client_state_path, "reading client state");
            let contents = tokio::fs::read(&self.client_state_path)
                .await
                .context("could not read wallet state file")?;
            tracing::trace!("deserializing client state");
            self.client_state = Some(
                serde_json::from_slice(&contents).context("could not deserialize wallet state")?,
            );
            tracing::trace!("finished deserializing client state");
        }
        Ok(self.client_state.as_mut().unwrap())
    }

    #[instrument(skip(self))]
    async fn save_state(&mut self) -> anyhow::Result<()> {
        if let Some(client_state) = &self.client_state {
            let time_since_last_save = self
                .last_saved
                .map(|i| i.elapsed())
                .unwrap_or(Duration::MAX);
            if time_since_last_save > self.save_interval {
                // Serialize the state to a temporary file
                let tmp_path = self.client_state_path.with_extension("tmp");
                let serialized =
                    serde_json::to_vec(client_state).context("could not serialize wallet state")?;
                let mut tmp_file = tokio::fs::OpenOptions::new()
                    .create(true)
                    .write(true)
                    .truncate(true)
                    .open(&tmp_path)
                    .await?;
                tmp_file.write_all(&serialized).await?;

                // Atomically move the temporary file to the final path
                tokio::fs::rename(&tmp_path, &self.client_state_path).await?;

                // We last saved now
                self.last_saved = Some(Instant::now());

                tracing::debug!("saved state");
            }
        }

        Ok(())
    }

    async fn lock_wallet(&self) -> anyhow::Result<fslock::LockFile> {
        let path = &self.client_state_path;

        let mut lock = fslock::LockFile::open(&path.with_extension("lock"))?;

        // Try to lock the file and note in the log if we are waiting for another process to finish
        tracing::debug!(?path, "locking wallet file");
        let lock = if !lock.try_lock()? {
            tracing::info!(?path, "waiting to acquire lock for wallet");
            tokio::task::spawn_blocking(move || {
                lock.lock()?;
                Ok::<_, anyhow::Error>(lock)
            })
            .await??
        } else {
            lock
        };

        Ok(lock)
    }

    /// Submits a transaction to the network, returning `Ok` only when the remote
    /// node has accepted the transaction, and erroring otherwise.
    #[instrument(skip(self, transaction))]
    pub async fn submit_transaction(&self, transaction: &Transaction) -> Result<(), anyhow::Error> {
        use penumbra_proto::Protobuf;
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

    async fn fetch_chain_params(&mut self) -> anyhow::Result<()> {
        if self.state().await?.chain_params().is_none() {
            *self.state().await?.chain_params_mut() = Some(
                self.light_wallet_client()
                    .await?
                    .chain_params(tonic::Request::new(ChainParamsRequest {
                        chain_id: self.state().await?.chain_id().unwrap_or_default(),
                    }))
                    .await?
                    .into_inner()
                    .into(),
            );
            self.save_state().await?;
            tracing::debug!("fetched chain parameters");
        }
        Ok(())
    }

    async fn update_asset_registry(&mut self) -> anyhow::Result<()> {
        let request = tonic::Request::new(AssetListRequest {
            chain_id: self.state().await?.chain_id().unwrap_or_default(),
        });
        let mut stream = self
            .thin_wallet_client()
            .await?
            .asset_list(request)
            .await?
            .into_inner();
        while let Some(asset) = stream.message().await? {
            self.state()
                .await?
                .asset_cache_mut()
                .extend(std::iter::once(
                    asset::REGISTRY
                        .parse_denom(&asset.asset_denom)
                        .ok_or_else(|| {
                            anyhow::anyhow!("invalid asset denomination: {}", asset.asset_denom)
                        })?,
                ));
        }
        self.save_state().await?;
        tracing::debug!("synced asset registry");
        Ok(())
    }

    async fn light_wallet_client(&self) -> Result<LightWalletClient<Channel>, anyhow::Error> {
        LightWalletClient::connect(format!("http://{}:{}", self.node, self.light_wallet_port))
            .await
            .map_err(Into::into)
    }

    async fn thin_wallet_client(&self) -> Result<ThinWalletClient<Channel>, anyhow::Error> {
        ThinWalletClient::connect(format!("http://{}:{}", self.node, self.thin_wallet_port))
            .await
            .map_err(Into::into)
    }
}
