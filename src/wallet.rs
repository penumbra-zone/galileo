use std::{collections::BTreeMap, path::PathBuf, time::Duration};

use anyhow::Context;
use derivative::Derivative;
use futures::StreamExt;
use penumbra_crypto::{
    keys::{SeedPhrase, SpendKey},
    Address, Value,
};
use penumbra_custody::CustodyClient;
use penumbra_proto::{
    chain::CompactBlock,
    client::oblivious::oblivious_query_client::ObliviousQueryClient,
    client::oblivious::{AssetListRequest, ChainParamsRequest, CompactBlockRangeRequest},
};
use penumbra_transaction::Transaction;
use penumbra_view::{Storage as ViewStorage, ViewClient, ViewService};
use penumbra_wallet::{build_transaction, plan};
use rand::rngs::OsRng;
use serde::{Deserialize, Serialize};
use tokio::{
    io::AsyncWriteExt,
    sync::{mpsc, oneshot, watch},
    time::{sleep, Instant},
};
use tonic::transport::Channel;
use tracing::instrument;

mod balance;
use balance::Balance;

/// The wallet worker, responsible for periodically synchronizing blocks from the chain and
/// transmitting transactions to the network.
#[derive(Derivative)]
// #[derivative(Debug)]
pub struct WalletWorker<V: ViewClient, C: CustodyClient> {
    view: V,
    custody: C,
    view_storage: Option<ViewStorage>,
    view_storage_path: PathBuf,
    wallet: Wallet,
    save_interval: Duration,
    sync_retries: u32,
    block_time_estimate: Duration,
    requests: Option<mpsc::Receiver<Request>>,
    sync: watch::Sender<bool>,
    node: String,
    pd_port: u16,
    rpc_port: u16,
    source: Option<u64>,
    last_saved: Option<Instant>,
    #[derivative(Debug = "ignore")]
    blocks: Option<tonic::Streaming<CompactBlock>>,
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
    /// Write the wallet data to the provided path.
    pub fn save(&self, path: impl AsRef<std::path::Path>) -> anyhow::Result<()> {
        if path.as_ref().exists() {
            return Err(anyhow::anyhow!(
                "Wallet file already exists, refusing to overwrite it"
            ));
        }
        use std::io::Write;
        let path = path.as_ref();
        let mut file = std::fs::File::create(path)?;
        let data = serde_json::to_vec(self)?;
        file.write_all(&data)?;
        Ok(())
    }

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
        view_storage_path: PathBuf,
        wallet: Wallet,
        source_address: Option<u64>,
        save_interval: Duration,
        block_time_estimate: Duration,
        buffer_size: usize,
        sync_retries: u32,
        node: String,
        pd_port: u16,
        rpc_port: u16,
    ) -> (mpsc::Sender<Request>, watch::Receiver<bool>, Self) {
        let (tx, rx) = mpsc::channel(buffer_size);
        let (watch_tx, watch_rx) = watch::channel(false);

        (
            tx,
            watch_rx,
            Self {
                view,
                custody,
                view_storage: None,
                view_storage_path,
                wallet,
                save_interval,
                sync_retries,
                block_time_estimate,
                requests: Some(rx),
                sync: watch_tx,
                node,
                pd_port,
                rpc_port,
                source: source_address,
                last_saved: None,
                blocks: None,
            },
        )
    }

    /// Run the wallet worker.
    pub async fn run(mut self) -> anyhow::Result<()> {
        let wallet_lock = self.lock_wallet().await?; // lock wallet file while running
        tracing::info!(
            "starting initial sync: please wait for sync to complete before requesting tokens"
        );

        if let Err(e) = self.initial_sync().await {
            tracing::error!(error = ?e, "initial sync error");
        }

        let mut requests = self.requests.take().unwrap();
        loop {
            tokio::select! {
                // Process requests
                request = requests.recv() => match request {
                    // TODO: do we need to block on sync here like the old galileo did?
                    Some(Request { destination, amounts, result, fee }) => {
                        tracing::trace!("sending back result of request");
                        let _ = result.send(self.dispense(destination, amounts, fee).await);
                    }
                    None => {
                        tracing::trace!("wallet request senders all dropped");
                        break;
                    }
                },
                // TODO: the view service should handle synchronization without anything special here
                // // Continuously synchronize
                // result = self.sync_one_block() => { result?; },
            }
        }
        drop(wallet_lock);
        Ok(())
    }

    /// Sync blocks until the sync interval times out, periodically flushing the state to disk.
    #[instrument(skip(self))]
    async fn initial_sync(&mut self) -> anyhow::Result<()> {
        let start = Instant::now();
        let fvk = self.wallet.spend_key.full_viewing_key();
        let mut status_stream = ViewClient::status_stream(&mut self.view, fvk.hash()).await?;

        // Pull out the first message from the stream, which has the current state, and use
        // it to set up a progress bar.
        let initial_status = status_stream
            .next()
            .await
            .transpose()?
            .ok_or_else(|| anyhow::anyhow!("view service did not report sync status"))?;

        println!(
            "Scanning blocks from last sync height {} to latest height {}",
            initial_status.sync_height, initial_status.latest_known_block_height,
        );

        let mut height = initial_status.sync_height;
        while let Some(status) = status_stream.next().await.transpose()? {
            println!("Scan status: {:#?}", status);
            height = status.sync_height;
        }

        tracing::info!(
            ?height,
            elapsed = ?start.elapsed(),
            "initial sync complete: ready to process requests"
        );

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
        // TODO: reimplement readiness checking and re-enable this loop
        // loop {
        // Get the current balance
        let balance = self.balance().await?;

        // Calculate whether the spend would be possible with our current balance
        let mut completely_ready = true;
        let mut completely_ready_with_change = true;

        for value in amounts.iter() {
            let ready = balance.ready.get(&value.asset_id).unwrap_or(&0);
            let change = balance.submitted_change.get(&value.asset_id).unwrap_or(&0);

            if value.amount > *ready {
                completely_ready = false;
            }

            if value.amount > ready + change {
                completely_ready_with_change = false;
            }
        }

        // If not completely ready, pause and retry
        // if !completely_ready {
        //     if completely_ready_with_change {
        //         tracing::debug!("waiting for change...");
        //         // TODO: remove??
        //         // self.sync_one_block().await?;
        //     } else {
        //         // TODO: this was being used for checking that change was returned from the transaction
        //         tracing::debug!("unimplemented: change checking");
        //         // tracing::warn!("not enough funds to complete transaction");
        //         // anyhow::bail!("not enough funds to complete transaction");
        //     }
        // } else {
        //     // If ready, proceed
        //     break;
        // }
        // }

        let source_address = self.source;

        let fvk = self.wallet.spend_key.full_viewing_key();
        let plan = plan::send(
            &fvk,
            &mut self.view,
            OsRng,
            &amounts,
            fee,
            destination,
            source_address,
            None,
        )
        .await?;

        let transaction =
            build_transaction(fvk, &mut self.view, &mut self.custody, OsRng, plan).await?;
        self.submit_transaction(&transaction).await?;
        // self.save_state().await?;
        Ok(())
    }

    // TODO: fix
    /// Get the current balance in the state (does not do synchronization).
    async fn balance(&mut self) -> anyhow::Result<Balance> {
        let source = self.source;
        // let mut unspent = self.state().await?.unspent_notes_by_address_and_denom();

        // // If only one source address is set, remove all the other notes
        // let unspent = if let Some(source) = source {
        //     BTreeMap::from_iter([(source, unspent.remove(&source).unwrap_or_default())].into_iter())
        // } else {
        //     unspent
        // };

        let mut balance = Balance::default();
        // for (_, per_address) in unspent {
        //     for (denom, notes) in per_address {
        //         for note in notes {
        //             use penumbra_wallet::UnspentNote::*;
        //             *match note {
        //                 Ready(_) => &mut balance.ready,
        //                 SubmittedSpend(_) => &mut balance.submitted_spend,
        //                 SubmittedChange(_) => &mut balance.submitted_change,
        //             }
        //             .entry(denom.id())
        //             .or_default() += note.as_ref().value().amount;
        //         }
        //     }
        // }

        Ok(balance)
    }

    // /// Get a mutable reference to the client state, loading it from disk if it has not previously
    // /// been loaded from disk.
    // ///
    // /// This will be slow the first time it is run, but every subsequent invocation will be fast.
    // async fn state(&mut self) -> anyhow::Result<&mut ClientState> {
    //     if self.client_state.is_none() {
    //         tracing::trace!(?self.client_state_path, "reading client state");
    //         let contents = tokio::fs::read(&self.client_state_path)
    //             .await
    //             .context("could not read wallet state file")?;
    //         tracing::trace!("deserializing client state");
    //         self.client_state = Some(
    //             serde_json::from_slice(&contents).context("could not deserialize wallet state")?,
    //         );
    //         tracing::trace!("finished deserializing client state");
    //     }
    //     Ok(self.client_state.as_mut().unwrap())
    // }

    /// Do expontential random backoff.
    async fn backoff(&self, retries: u32) {
        // Exponential backoff up to the retry number
        let delay = self.block_time_estimate * 2u32.pow(retries);
        let jitter = Duration::from_millis(rand::random::<u64>() % 1000);
        tokio::time::sleep(delay + jitter).await;
    }

    // TODO: i think this is unnecessary with the new ViewService
    /// Scan the next available block into the wallet state.
    ///
    /// Returns `true` if it has reached the top of the chain.
    // #[instrument(skip(self))]
    // async fn sync_one_block(&mut self) -> anyhow::Result<bool> {
    //     let mut retries: u32 = 0;

    //     let finished = loop {
    //         match self.blocks().await?.message().await {
    //             Ok(Some(block)) => {
    //                 if block.height % 1000 == 0 {
    //                     tracing::info!(height = ?block.height, "syncing...");
    //                 } else {
    //                     tracing::debug!(height = ?block.height, "syncing...");
    //                 }
    //                 self.state()
    //                     .await?
    //                     .scan_block(block.try_into()?)
    //                     .context("invalid block when scanning")?;
    //                 self.save_state().await?;
    //                 break false;
    //             }
    //             Ok(None) => {
    //                 self.blocks = None;
    //                 break true;
    //             }
    //             Err(error) => {
    //                 // Try a fresh request if we retry
    //                 self.blocks = None;

    //                 if error.code() == tonic::Code::Unavailable {
    //                     if retries < self.sync_retries {
    //                         tracing::warn!(?error, "error syncing block");
    //                         retries += 1;
    //                         self.backoff(retries).await;
    //                     } else {
    //                         tracing::error!(?error, ?retries, "error syncing block after retrying");
    //                         anyhow::bail!(error);
    //                     }
    //                 } else {
    //                     tracing::error!(?error, "error syncing block");
    //                     anyhow::bail!(error);
    //                 }
    //             }
    //         }
    //     };

    //     let _ = self.sync.send(finished); // Notify that there's more state, so notes may be available
    //     Ok(finished)
    // }

    // TODO: i think this is unnecessary now as well
    /// Get a stream of compact blocks starting at the current wallet height, making a new request
    /// if necessary.
    // async fn blocks(&mut self) -> anyhow::Result<&mut tonic::Streaming<CompactBlock>> {
    //     let mut retries = 0;

    //     if self.blocks.is_none() {
    //         tracing::debug!("making new block stream request");

    //         let start_height = self
    //             .state()
    //             .await?
    //             .last_block_height()
    //             .map(|h| h + 1)
    //             .unwrap_or(0);

    //         let stream = loop {
    //             match self
    //                 .oblivious_query_client()
    //                 .await?
    //                 .compact_block_range(tonic::Request::new(CompactBlockRangeRequest {
    //                     start_height,
    //                     end_height: 0,
    //                     keep_alive: true,
    //                     chain_id: self.view.chain_params().await?.chain_id,
    //                 }))
    //                 .await
    //                 .map(|r| r.into_inner())
    //             {
    //                 Ok(stream) => break stream,
    //                 Err(error) => {
    //                     if error.code() == tonic::Code::Unavailable {
    //                         if retries < self.sync_retries {
    //                             tracing::warn!(?error, "error acquiring block stream");
    //                             retries += 1;
    //                             self.backoff(retries).await;
    //                         } else {
    //                             tracing::error!(
    //                                 ?error,
    //                                 ?retries,
    //                                 "error acquiring block stream after retrying"
    //                             );
    //                             anyhow::bail!(error);
    //                         }
    //                     } else {
    //                         tracing::error!(?error, "error acquiring block stream");
    //                         anyhow::bail!(error);
    //                     }
    //                 }
    //             }
    //         };

    //         self.blocks = Some(stream);
    //     }

    //     Ok(self.blocks.as_mut().unwrap())
    // }

    // TODO: we might want to resurrect this for some Galileo-specific state, for example mapping of Discord users
    // to addresses.
    // /// Save the current state to disk, if the last save-time was sufficiently far in the past.
    // ///
    // /// This does not definitely save the state; it may be used liberally, wherever the state is
    // /// changed, because it only does something if the time since the last actual save is greater
    // /// than the save interval.
    // #[instrument(skip(self))]
    // async fn save_state(&mut self) -> anyhow::Result<()> {
    //     if let Some(client_state) = &self.client_state {
    //         let time_since_last_save = self
    //             .last_saved
    //             .map(|i| i.elapsed())
    //             .unwrap_or(Duration::MAX);
    //         if time_since_last_save > self.save_interval {
    //             tracing::debug!("saving state");

    //             // Serialize the state to a temporary file
    //             let tmp_path = self.client_state_path.with_extension("tmp");
    //             let serialized =
    //                 serde_json::to_vec(client_state).context("could not serialize wallet state")?;
    //             let mut tmp_file = tokio::fs::OpenOptions::new()
    //                 .create(true)
    //                 .write(true)
    //                 .truncate(true)
    //                 .open(&tmp_path)
    //                 .await
    //                 .context("could not open temp file while saving state")?;
    //             tmp_file
    //                 .write_all(&serialized)
    //                 .await
    //                 .context("could not write state to temp file")?;

    //             // Atomically move the temporary file to the final path
    //             tokio::fs::rename(&tmp_path, &self.client_state_path)
    //                 .await
    //                 .context("could not overwrite existing state file")?;

    //             // We last saved now
    //             self.last_saved = Some(Instant::now());

    //             tracing::debug!("saved state");
    //         }
    //     }

    //     Ok(())
    // }

    /// Lock the wallet file, returning a [`fslock::LockFile`] object that will hold the lock until
    /// dropped.
    ///
    /// This should be called before changing the contents of the wallet file, to prevent other
    /// processes from stomping on our changes (i.e. concurrent use of `pcli`).
    async fn lock_wallet(&self) -> anyhow::Result<fslock::LockFile> {
        // TODO: do we need to do this for the custody path as well?
        let path = &self.view_storage_path;

        let mut lock = fslock::LockFile::open(&path.with_extension("lock"))
            .context("could not open wallet file lock")?;

        // Try to lock the file and note in the log if we are waiting for another process to finish
        tracing::debug!(?path, "locking wallet file");
        let lock = if !lock.try_lock()? {
            tracing::info!(?path, "waiting to acquire lock for wallet");
            tokio::task::spawn_blocking(move || {
                lock.lock()?;
                Ok::<_, anyhow::Error>(lock)
            })
            .await
            .context("panic while trying to lock wallet file")?
            .context("could not lock wallet file")?
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

    /// Make a new oblivious query client and return it.
    async fn oblivious_query_client(&self) -> Result<ObliviousQueryClient<Channel>, anyhow::Error> {
        ObliviousQueryClient::connect(format!("http://{}:{}", self.node, self.pd_port))
            .await
            .context("could not connect light wallet client")
    }
}
