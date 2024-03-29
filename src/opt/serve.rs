use anyhow::Context;
use clap::Parser;
use directories::ProjectDirs;
use futures::stream::FuturesUnordered;
use futures_util::{stream::StreamExt, stream::TryStreamExt};
use num_traits::identities::Zero;
use penumbra_asset::Value;
use penumbra_custody::soft_kms::SoftKms;
use penumbra_proto::{
    custody::v1::{
        custody_service_client::CustodyServiceClient, custody_service_server::CustodyServiceServer,
    },
    view::v1::{view_service_client::ViewServiceClient, view_service_server::ViewServiceServer},
};
use penumbra_view::{ViewClient, ViewServer};
use serenity::prelude::GatewayIntents;
use std::{env, path::PathBuf, time::Duration};
use tokio::sync::mpsc;
use tower::limit::concurrency::ConcurrencyLimit;
use tower::{balance as lb, load};
use url::Url;

use crate::{
    opt::ChannelIdAndMessageId, responder::RequestQueue, sender::SenderSet, Catchup, Handler,
    Responder, Sender, Wallet,
};

#[derive(Debug, Clone, Parser)]
pub struct Serve {
    /// The transaction fee for each response (paid in upenumbra).
    #[structopt(long, default_value = "0")]
    fee: u64,
    /// Per-user rate limit (e.g. "10m" or "1day").
    #[clap(short, long, default_value = "1day", parse(try_from_str = humantime::parse_duration))]
    rate_limit: Duration,
    /// Maximum number of times to reply to a user informing them of the rate limit.
    #[clap(long, default_value = "5")]
    reply_limit: usize,
    /// Maximum number of addresses per message to which to dispense tokens.
    #[clap(long, default_value = "1")]
    max_addresses: usize,
    /// Number of accounts to send funds from. Funds will send from account indices [0, n-1].
    #[clap(long, default_value = "4")]
    account_count: u32,
    /// Path to the directory to use to store data [default: platform appdata directory].
    #[clap(long, short)]
    data_dir: Option<PathBuf>,
    /// The URL of the pd gRPC endpoint on the remote node.
    #[clap(short, long, default_value = "https://grpc.testnet.penumbra.zone")]
    node: Url,
    /// Message ID of an as-yet unhonored fund request. Will scan
    /// all messages including and since the one specified. Should be
    /// specified as a full URL to a specific Discord message.
    #[clap(long)]
    catch_up_after: Vec<ChannelIdAndMessageId>,
    /// Batch size for responding to catch-up backlog.
    #[clap(long, default_value = "25")]
    catch_up_batch_size: usize,
    /// Disable transaction sending and Discord notifications. Useful for debugging.
    #[clap(long)]
    dry_run: bool,
    /// The amounts to send for each response, written as typed values 1.87penumbra, 12cubes, etc.
    values: Vec<Value>,
}

impl Serve {
    /// Run the bot, listening for relevant messages, and responding as appropriate.
    pub async fn exec(self) -> anyhow::Result<()> {
        self.preflight_checks()
            .await
            .context("failed preflight checks")?;

        let discord_token = env::var("DISCORD_TOKEN")?;

        // Configure custody for Penumbra wallet key material.
        // Look up the path to the view state file per platform, creating the directory if needed
        let data_dir = self.data_dir.unwrap_or_else(|| {
            ProjectDirs::from("zone", "penumbra", "pcli")
                .expect("can access penumbra project dir")
                .data_dir()
                .to_owned()
        });
        std::fs::create_dir_all(&data_dir).context("can create data dir")?;
        tracing::debug!(?data_dir, "loading custody key material");

        let pcli_config_file = data_dir.clone().join("config.toml");
        let wallet = Wallet::load(pcli_config_file)
            .context("failed to load wallet from local custody file")?;
        let soft_kms = SoftKms::new(wallet.spend_key.clone().into());
        let custody = CustodyServiceClient::new(CustodyServiceServer::new(soft_kms));
        let fvk = wallet.spend_key.full_viewing_key().clone();

        // Initialize view client, to scan Penumbra chain.
        let view_file = data_dir.clone().join("pcli-view.sqlite");
        tracing::debug!("configuring ViewService against node {}", &self.node);
        let view_filepath = Some(
            view_file
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("Non-UTF8 view path"))?
                .to_string(),
        );
        let view_storage =
            penumbra_view::Storage::load_or_initialize(view_filepath, &fvk, self.node.clone())
                .await?;
        let view_service = ViewServer::new(view_storage, self.node.clone()).await?;

        // Now build the view and custody clients, doing gRPC with ourselves
        let mut view = ViewServiceClient::new(ViewServiceServer::new(view_service));

        // Wait to synchronize the chain before doing anything else.
        tracing::info!(
            "starting initial sync: please wait for sync to complete before requesting tokens"
        );
        ViewClient::status_stream(&mut view)
            .await?
            .try_collect::<Vec<_>>()
            .await?;
        // From this point on, the view service is synchronized.
        tracing::info!("initial sync complete");

        // Instantiate a sender for each account index.
        let mut senders = vec![];
        for account_id in 0..self.account_count {
            let sender = Sender::new(account_id, fvk.clone(), view.clone(), custody.clone());
            senders.push(sender);
        }

        let d = SenderSet::new(
            senders
                .into_iter()
                .enumerate()
                .map(|(k, v)| (k as u32, v))
                .collect(),
        );
        let lb = lb::p2c::Balance::new(load::PendingRequestsDiscover::new(
            d,
            load::CompleteOnResponse::default(),
        ));
        let service = ConcurrencyLimit::new(lb, self.account_count.try_into().unwrap());

        // Make a worker to handle the address queue
        let (send_requests, responder) = Responder::new(service, self.max_addresses, self.values);

        // Create a watcher for Discord messages, which will manage spends and replies.
        let handler = Handler::new(self.rate_limit, self.reply_limit, self.dry_run);

        tracing::debug!("configuring discord client");
        // Make a new client using a token set by an environment variable, with our handlers
        let mut discord_client = serenity::Client::builder(
            &discord_token,
            GatewayIntents::non_privileged() | GatewayIntents::MESSAGE_CONTENT,
        )
        .event_handler(handler)
        .await?;

        // Put the sending end of the address queue into the global TypeMap
        discord_client
            .data
            .write()
            .await
            .insert::<RequestQueue>(send_requests.clone());

        // Make a separate catch-up worker for each catch-up task, and collect their results (first
        // to fail kills the bot)
        let http = discord_client.cache_and_http.http.clone();
        let catch_up = tokio::spawn(async move {
            let mut catch_ups: FuturesUnordered<_> = self
                .catch_up_after
                .into_iter()
                .map(
                    |ChannelIdAndMessageId {
                         channel_id,
                         message_id,
                     }| {
                        let catch_up = Catchup::new(
                            channel_id,
                            self.catch_up_batch_size,
                            http.clone(),
                            send_requests.clone(),
                        );
                        tokio::spawn(catch_up.run(message_id))
                    },
                )
                .collect();

            // By default, `--catch-up` is empty, so this logic won't run.
            if !catch_ups.is_empty() {
                tracing::debug!("attempting to catch-up on indicated channels");
            }
            while let Some(result) = catch_ups.next().await {
                result??;
            }

            // Wait forever
            std::future::pending().await
        });

        let (cancel_tx, mut cancel_rx) = mpsc::channel(1);

        // Start the client and the two workers
        tokio::select! {
            result = tokio::spawn(async move { discord_client.start().await }) =>
                result.unwrap().context("error in discord client service"),
            result = tokio::spawn(async move { responder.run(cancel_tx).await }) =>
                result.unwrap().context("error in responder service"),
            result = catch_up => result.context("error in catchup service")?,
            _ = cancel_rx.recv() => {
                // Cancellation received
                Err(anyhow::anyhow!("cancellation received"))
            }
        }
    }
    /// Perform sanity checks on CLI args prior to running.
    async fn preflight_checks(&self) -> anyhow::Result<()> {
        if self.dry_run {
            tracing::info!("dry-run mode is enabled, won't send transactions or post messages");
        }
        if self.values.is_empty() {
            anyhow::bail!("at least one value must be provided");
        } else if self.values.iter().any(|v| v.amount.value().is_zero()) {
            anyhow::bail!("all values must be non-zero");
        }

        tracing::debug!("checking discord token...");
        let _discord_token =
            env::var("DISCORD_TOKEN").context("missing environment variable DISCORD_TOKEN")?;

        // TODO: i think the serenity token validation logic has a bug somewhere because it always
        // fails with seemingly correct tokens:
        // https://docs.rs/serenity/0.11.5/src/serenity/utils/token.rs.html
        // if token::validate(discord_token.clone()).is_err() {
        //     anyhow::bail!("invalid discord token");
        // }
        Ok(())
    }
}
