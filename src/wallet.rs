use std::{path::PathBuf, time::Duration};

use anyhow::Context;
use penumbra_crypto::{Address, Value};
use penumbra_proto::light_wallet::{
    light_wallet_client::LightWalletClient, CompactBlockRangeRequest,
};
use penumbra_wallet::ClientState;
use tokio::{
    io::AsyncWriteExt,
    sync::{mpsc, oneshot},
    time::{self, Instant},
};
use tonic::transport::Channel;
use tracing::instrument;

#[derive(Debug)]
pub struct Wallet {
    client_state: Option<ClientState>,
    client_state_path: PathBuf,
    sync_interval: Duration,
    requests: mpsc::Receiver<Request>,
    initial_sync: bool,
    node: String,
    light_wallet_port: u16,
}

#[derive(Debug)]
pub struct Request {
    destination: Address,
    amounts: Vec<Value>,
    result: oneshot::Sender<Result<(), anyhow::Error>>,
}

impl Request {
    pub fn send(
        destination: Address,
        amounts: Vec<Value>,
    ) -> (oneshot::Receiver<anyhow::Result<()>>, Self) {
        let (tx, rx) = oneshot::channel();
        let request = Request {
            destination,
            amounts,
            result: tx,
        };
        (rx, request)
    }
}

impl Wallet {
    pub fn new(
        client_state_path: PathBuf,
        sync_interval: Duration,
        buffer_size: usize,
        node: String,
        light_wallet_port: u16,
    ) -> (mpsc::Sender<Request>, Self) {
        let (tx, rx) = mpsc::channel(buffer_size);
        (
            tx,
            Self {
                client_state: None,
                client_state_path,
                sync_interval,
                requests: rx,
                initial_sync: false,
                node,
                light_wallet_port,
            },
        )
    }

    pub async fn run(mut self) -> anyhow::Result<()> {
        let mut interval = time::interval(self.sync_interval);
        let wallet_lock = self.lock_wallet().await?; // lock wallet file while running
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    tracing::trace!("syncing wallet");
                    self.sync().await?;
                },
                request = self.requests.recv() => match request {
                    Some(Request { destination, amounts, result }) => if self.initial_sync {
                        tracing::trace!("sending back result of request");
                        let _ = result.send(self.dispense(destination, amounts).await);
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
        let start = Instant::now();
        tracing::trace!("starting sync");

        let mut client = self.light_wallet_client().await?;
        let sync_interval = self.sync_interval;
        let state = self.state().await?;

        let start_height = state.last_block_height().map(|h| h + 1).unwrap_or(0);
        let mut stream = client
            .compact_block_range(tonic::Request::new(CompactBlockRangeRequest {
                start_height,
                end_height: 0,
                chain_id: state
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
                state.scan_block(block)?;
                count += 1;
            } else {
                tracing::debug!(height = ?state.last_block_height(), ?count, "finished sync");
                self.initial_sync = true;
                return Ok(());
            }
        }

        tracing::debug!(
            height = ?state.last_block_height().unwrap_or(0),
            ?count,
            "sync continuation queued for next interval"
        );

        tracing::trace!("saving client state");
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

    async fn save_state(&self) -> anyhow::Result<()> {
        if let Some(client_state) = &self.client_state {
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
        }

        Ok(())
    }

    async fn lock_wallet(&self) -> anyhow::Result<fslock::LockFile> {
        let path = &self.client_state_path;

        let mut lock = fslock::LockFile::open(&path.with_extension("lock"))?;

        // Try to lock the file and note in the log if we are waiting for another process to finish
        tracing::debug!(?path, "Locking wallet file");
        let lock = if !lock.try_lock()? {
            tracing::info!(?path, "Waiting to acquire lock for wallet");
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

    pub async fn light_wallet_client(&self) -> Result<LightWalletClient<Channel>, anyhow::Error> {
        LightWalletClient::connect(format!("http://{}:{}", self.node, self.light_wallet_port))
            .await
            .map_err(Into::into)
    }

    #[instrument(skip(self))]
    async fn dispense(&mut self, destination: Address, amounts: Vec<Value>) -> anyhow::Result<()> {
        // TODO: prepare a transaction and transmit it to the network, then wait for it to be confirmed.
        Ok(())
    }
}
