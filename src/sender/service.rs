//! Facilities to abstract over a [`SenderSet`] as a load-balanced [`tower::Service`].
//!
//! See [`init()`].

use {
    super::{Sender, SenderRequest, SenderSet},
    crate::Wallet,
    anyhow::Context,
    directories::ProjectDirs,
    futures_util::stream::TryStreamExt,
    penumbra_custody::{soft_kms::SoftKms, CustodyClient},
    penumbra_keys::FullViewingKey,
    penumbra_proto::{
        custody::v1::{
            custody_service_client::CustodyServiceClient,
            custody_service_server::CustodyServiceServer,
        },
        view::v1::{
            view_service_client::ViewServiceClient, view_service_server::ViewServiceServer,
        },
    },
    penumbra_view::{ViewClient, ViewServer},
    std::path::Path,
    tower::{
        balance::p2c::Balance, limit::concurrency::ConcurrencyLimit, load::PendingRequestsDiscover,
    },
    url::Url,
};

/// Constructs a [`tower::Service`] of [`Sender`]s mapping requests to transaction hashes.
/* TODO(kate): the responder constructor hard-codes this return type. fix it later. */
pub async fn init(
    data_dir: Option<&Path>,
    account_count: u32,
    node: &Url,
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
    let d = senders_iter(data_dir, account_count, node)
        .await
        .map(SenderSet::new)?;

    // Distribute requests across the senders.
    let lb = Balance::new(PendingRequestsDiscover::new(
        d,
        tower::load::CompleteOnResponse::default(),
    ));

    // Limit the concurrent requests to 1-per-account.
    let account_count = account_count
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
    data_dir: Option<&Path>,
    account_count: u32,
    node: &Url,
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
    let data_dir = data_dir.map(Path::to_path_buf).unwrap_or_else(|| {
        ProjectDirs::from("zone", "penumbra", "pcli")
            .expect("can access penumbra project dir")
            .data_dir()
            .to_owned()
    });
    std::fs::create_dir_all(&data_dir).context("can create data dir")?;
    tracing::debug!(?data_dir, "loading custody key material");

    // Initialize a custody service client and a view service client for the senders.
    let (custody, fvk) = initialize_custody_service(&data_dir)?;
    let view = initialize_view_service(node, &data_dir, &fvk).await?;

    let make_sender = move |id| {
        let sender = Sender::new(id, fvk.clone(), view.clone(), custody.clone());
        (id, sender)
    };

    // Instantiate a sender for each account index.
    Ok((0..account_count).map(make_sender))
}

/// Initializes a custody client, to request spend authorization.
fn initialize_custody_service(
    data_dir: &Path,
) -> anyhow::Result<(impl CustodyClient + Clone + Send + 'static, FullViewingKey)> {
    let pcli_config_file = data_dir.join("config.toml");
    let wallet =
        Wallet::load(pcli_config_file).context("failed to load wallet from local custody file")?;
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
