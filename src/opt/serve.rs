use clap::Parser;
use directories::ProjectDirs;
use penumbra_crypto::Value;
use std::{env, path::PathBuf, time::Duration};

use crate::{responder::RequestQueue, Handler, Responder, Wallet};

#[derive(Debug, Clone, Parser)]
pub struct Serve {
    /// The transaction fee for each response (paid in upenumbra).
    #[structopt(long, default_value = "0")]
    fee: u64,
    /// Per-user rate limit (e.g. "10m" or "1day").
    #[clap(short, long, default_value = "1h", parse(try_from_str = humantime::parse_duration))]
    rate_limit: Duration,
    /// Maximum number of times to reply to a user informing them of the rate limit.
    #[clap(long, default_value = "5")]
    reply_limit: usize,
    /// Interval at which to synchronize the bot-owned wallet from the chain.
    #[clap(short, long = "sync", default_value = "10s", parse(try_from_str = humantime::parse_duration))]
    sync_interval: Duration,
    /// Maximum number of addresses per message to which to dispense tokens.
    #[clap(long, default_value = "1")]
    max_addresses: usize,
    /// Internal buffer size for the queue of actions to perform.
    #[clap(long, default_value = "100")]
    buffer_size: usize,
    /// Path to the wallet file to use [default: platform appdata directory].
    #[clap(long, short)]
    wallet_file: Option<PathBuf>,
    /// The address of the pd+tendermint node.
    #[clap(short, long, default_value = "testnet.penumbra.zone")]
    node: String,
    /// The port to use to speak to pd's light wallet server.
    #[clap(long, default_value = "26666")]
    light_wallet_port: u16,
    /// The port to use to speak to pd's thin wallet server.
    #[clap(long, default_value = "26667")]
    thin_wallet_port: u16,
    /// The port to use to speak to tendermint.
    #[clap(long, default_value = "26657")]
    rpc_port: u16,
    #[clap(long = "source")]
    source_address: Option<u64>,
    /// The amounts to send for each response, written as typed values 1.87penumbra, 12cubes, etc.
    values: Vec<Value>,
}

impl Serve {
    pub async fn exec(self) -> anyhow::Result<()> {
        let discord_token = env::var("DISCORD_TOKEN")?;

        let handler = Handler::new(self.rate_limit, self.reply_limit);

        // Make a new client using a token set by an environment variable, with our handlers
        let mut client = serenity::Client::builder(&discord_token)
            .event_handler(handler)
            .await?;

        // Look up the path to the wallet file per platform, creating the directory if needed
        let wallet_file = self.wallet_file.map_or_else(
            || {
                let project_dir = ProjectDirs::from("zone", "penumbra", "pcli")
                    .expect("can access penumbra project dir");
                // Currently we use just the data directory. Create it if it is missing.
                std::fs::create_dir_all(project_dir.data_dir())
                    .expect("can create penumbra data directory");
                project_dir.data_dir().join("penumbra_wallet.json")
            },
            PathBuf::from,
        );

        // Make a worker to handle the wallet
        let (wallet_requests, wallet) = Wallet::new(
            wallet_file,
            self.source_address,
            self.sync_interval,
            self.buffer_size,
            self.node,
            self.light_wallet_port,
            self.thin_wallet_port,
            self.rpc_port,
        );

        // Make a worker to handle the address queue
        let (send_requests, responder) = Responder::new(
            wallet_requests,
            self.max_addresses,
            self.buffer_size,
            self.values,
            self.fee,
        );

        // Put the sending end of the address queue into the global TypeMap
        client
            .data
            .write()
            .await
            .insert::<RequestQueue>(send_requests);

        // Start the client and the worker
        tokio::select! {
            result = client.start() => result.map_err(Into::into),
            result = tokio::spawn(responder.run()) => result?,
            result = tokio::spawn(wallet.run()) => result?,
        }
    }
}
