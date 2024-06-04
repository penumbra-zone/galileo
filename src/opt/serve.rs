use anyhow::Context;
use clap::Parser;
use directories::ProjectDirs;
use futures::stream::FuturesUnordered;
use futures_util::{stream::StreamExt, stream::TryStreamExt};
use num_traits::identities::Zero;
use penumbra_asset::Value;
use penumbra_custody::{soft_kms::SoftKms, CustodyClient};
use penumbra_keys::FullViewingKey;
use penumbra_proto::{
    custody::v1::{
        custody_service_client::CustodyServiceClient, custody_service_server::CustodyServiceServer,
    },
    view::v1::{view_service_client::ViewServiceClient, view_service_server::ViewServiceServer},
};
use penumbra_view::{ViewClient, ViewServer};
use serenity::{http::Http, prelude::GatewayIntents};
use std::{
    env,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::mpsc;
use tower::{
    balance::p2c::Balance, limit::concurrency::ConcurrencyLimit, load::PendingRequestsDiscover,
};
use url::Url;

use crate::{
    opt::ChannelIdAndMessageId,
    responder::RequestQueue,
    sender::{SenderRequest, SenderSet},
    Catchup, Handler, Responder, Sender, Wallet,
};

#[derive(Debug, Clone, Parser)]
pub struct Serve {
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

        // Make a worker to handle the address queue
        let service = self.senders_service().await?;
        let (send_requests, responder) =
            Responder::new(service, self.max_addresses, self.values.clone());

        // Make a worker to run the discord client
        let mut discord_client = self
            .discord_worker(discord_token, send_requests.clone())
            .await?;
        let http = discord_client.cache_and_http.http.clone();
        let discord = tokio::spawn(async move { discord_client.start().await });

        // Make a separate catch-up worker for each catch-up task, and collect their results (first
        // to fail kills the bot)
        let catch_up = tokio::spawn(Self::catch_up_worker(
            self.catch_up_after,
            self.catch_up_batch_size,
            http,
            send_requests,
        ));

        let (cancel_tx, mut cancel_rx) = mpsc::channel(1);

        // Start the client and the two workers
        tokio::select! {
            result = discord => result.unwrap().context("error in discord client service"),
            result = tokio::spawn(async move { responder.run(cancel_tx).await }) =>
                result.unwrap().context("error in responder service"),
            result = catch_up => result.context("error in catchup service")?,
            _ = cancel_rx.recv() => {
                // Cancellation received
                Err(anyhow::anyhow!("cancellation received"))
            }
        }
    }

    /// Constructs a [`tower::Service`] of [`Sender`]s mapping requests to transaction hashes.
    /* TODO(kate): the responder constructor hard-codes this return type. fix it later. */
    async fn senders_service(
        &self,
    ) -> anyhow::Result<
        ConcurrencyLimit<
            Balance<
                PendingRequestsDiscover<
                    SenderSet<
                        ConcurrencyLimit<
                            Sender<
                                impl ViewClient + Clone + 'static,
                                impl CustodyClient + Clone + 'static,
                            >,
                        >,
                    >,
                >,
                SenderRequest,
            >,
        >,
    > {
        // Create a sender set, with one sender for each account.
        let d = self.senders_iter().await.map(SenderSet::new)?;

        // Distribute requests across the senders.
        let lb = Balance::new(PendingRequestsDiscover::new(
            d,
            tower::load::CompleteOnResponse::default(),
        ));

        // Limit the concurrent requests to 1-per-account.
        let account_count = self
            .account_count
            .try_into()
            .expect("number of accouts < u32::MAX");
        let service = ConcurrencyLimit::new(lb, account_count);

        Ok(service)
    }

    /// Returns an [`Iterator`] that will emit a [`Sender`] for each account index.
    ///
    /// This initializes a custody server and a view server that the senders will use to create and
    /// authorize spends.
    async fn senders_iter(
        &self,
    ) -> anyhow::Result<
        impl Iterator<
            Item = (
                u32, // the account id
                ConcurrencyLimit<
                    Sender<
                        // a rate-limited sender
                        impl ViewClient + Clone + 'static,
                        impl CustodyClient + Clone + 'static,
                    >,
                >,
            ),
        >,
    > {
        // Configure custody for Penumbra wallet key material.
        // Look up the path to the view state file per platform, creating the directory if needed
        let data_dir = self.data_dir.clone().unwrap_or_else(|| {
            ProjectDirs::from("zone", "penumbra", "pcli")
                .expect("can access penumbra project dir")
                .data_dir()
                .to_owned()
        });
        std::fs::create_dir_all(&data_dir).context("can create data dir")?;
        tracing::debug!(?data_dir, "loading custody key material");

        // Initialize a custody service client and a view service client for the senders.
        let (custody, fvk) = Self::initialize_custody_service(&data_dir)?;
        let view = Self::initialize_view_service(&self.node, &data_dir, &fvk).await?;

        let make_sender = move |id| {
            let sender = Sender::new(id, fvk.clone(), view.clone(), custody.clone());
            (id, sender)
        };

        // Instantiate a sender for each account index.
        Ok((0..self.account_count).map(make_sender))
    }

    /// Initializes a custody client, to request spend authorization.
    fn initialize_custody_service(
        data_dir: &Path,
    ) -> anyhow::Result<(impl CustodyClient + Clone + Send + 'static, FullViewingKey)> {
        let pcli_config_file = data_dir.join("config.toml");
        let wallet = Wallet::load(pcli_config_file)
            .context("failed to load wallet from local custody file")?;
        let soft_kms = SoftKms::new(wallet.spend_key.clone().into());
        let custody = CustodyServiceClient::new(CustodyServiceServer::new(soft_kms));
        let fvk = wallet.spend_key.full_viewing_key().clone();
        Ok((custody, fvk))
    }

    // Initializes a view client, to scan the Penumbra chain.
    //
    // This will build and synchronize the view service before it returns the client.
    async fn initialize_view_service(
        node: &Url,
        data_dir: &Path,
        fvk: &FullViewingKey,
    ) -> anyhow::Result<impl ViewClient + Clone + Send + 'static> {
        let view_file = data_dir.join("pcli-view.sqlite");
        tracing::debug!("configuring ViewService against node {}", &node);
        let view_filepath = Some(
            view_file
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("Non-UTF8 view path"))?
                .to_string(),
        );
        let view_storage =
            penumbra_view::Storage::load_or_initialize(view_filepath, &fvk, node.clone()).await?;
        let view_service = ViewServer::new(view_storage, node.clone()).await?;

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

        Ok(view)
    }

    /// Configures a new discord [`Client`][serenity::Client].
    ///
    /// This will configure a [`Handler`] to handle events, and will send requests for funds
    /// that it sees to the provided [`mpsc`] channel.
    async fn discord_worker(
        &self,
        token: impl AsRef<str>,
        request_tx: mpsc::Sender<crate::responder::Request>,
    ) -> anyhow::Result<serenity::Client> {
        let Self {
            rate_limit,
            reply_limit,
            dry_run,
            ..
        } = *self;

        // Create a watcher for Discord messages, which will manage spends and replies.
        let handler = Handler::new(rate_limit, reply_limit, dry_run);

        tracing::debug!("configuring discord client");
        // Make a new client using a token set by an environment variable, with our handlers
        let discord_client = serenity::Client::builder(
            &token,
            GatewayIntents::non_privileged() | GatewayIntents::MESSAGE_CONTENT,
        )
        .event_handler(handler)
        .await?;

        // Put the sending end of the address queue into the global TypeMap
        discord_client
            .data
            .write()
            .await
            .insert::<RequestQueue>(request_tx);

        Ok(discord_client)
    }

    async fn catch_up_worker(
        catch_up_after: Vec<ChannelIdAndMessageId>,
        catch_up_batch_size: usize,
        http: Arc<Http>,
        send_requests: mpsc::Sender<crate::responder::Request>,
    ) -> Result<(), anyhow::Error> {
        let mut catch_ups: FuturesUnordered<_> = catch_up_after
            .into_iter()
            .map(
                |ChannelIdAndMessageId {
                     channel_id,
                     message_id,
                 }| {
                    let catch_up = Catchup::new(
                        channel_id,
                        catch_up_batch_size,
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
