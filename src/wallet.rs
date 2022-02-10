use std::{path::PathBuf, time::Duration};

use penumbra_crypto::{Address, Value};
use penumbra_wallet::ClientState;
use tokio::{
    sync::{mpsc, oneshot},
    time,
};
use tracing::instrument;

pub struct Wallet {
    client_state: Option<ClientState>,
    client_state_path: PathBuf,
    sync_interval: Duration,
    requests: mpsc::Receiver<Request>,
}

pub struct Request {
    destination: Address,
    amounts: Vec<Value>,
    result: oneshot::Sender<Result<(), anyhow::Error>>,
}

impl Wallet {
    pub async fn new(
        client_state_path: PathBuf,
        sync_interval: Duration,
        buffer_size: usize,
    ) -> (mpsc::Sender<Request>, Self) {
        let (tx, rx) = mpsc::channel(buffer_size);
        (
            tx,
            Self {
                client_state: None,
                client_state_path,
                sync_interval,
                requests: rx,
            },
        )
    }

    pub async fn run(mut self) {
        let mut interval = time::interval(self.sync_interval);
        loop {
            tokio::select! {
                _ = interval.tick() => {
                    tracing::trace!("syncing wallet");
                    self.sync().await
                },
                request = self.requests.recv() => match request {
                    Some(Request { destination, amounts, result }) => {
                        tracing::trace!("sending back result of request");
                        let _ = result.send(self.dispense(destination, amounts).await);
                    }
                    None => {
                        tracing::trace!("wallet request senders all dropped");
                        break;
                    }
                },
            }
        }
    }

    #[instrument(skip(self))]
    async fn sync(&mut self) {}

    #[instrument(skip(self))]
    async fn dispense(
        &mut self,
        destination: Address,
        amounts: Vec<Value>,
    ) -> Result<(), anyhow::Error> {
        Ok(())
    }
}
