use {
    crate::Responder,
    anyhow::{anyhow, Context},
    clap::Parser,
    penumbra_asset::Value,
    std::{net::SocketAddr, path::PathBuf},
    tap::Tap,
};

/// Run galileo on an RPC endpoint.
#[derive(Debug, Clone, Parser)]
pub struct ServeRpc {
    /// The number of accounts to send funds from. Funds will send from account indices `[0, n-1]`.
    #[clap(long, default_value = "4")]
    account_count: u32,
    /// The path to the directory to use to store data [default: platform appdata directory].
    #[clap(long, short)]
    data_dir: Option<PathBuf>,
    /// The URL of the pd gRPC endpoint on the remote node.
    ///
    /// This is the node that transactions will be sent to.
    #[clap(short, long, default_value = "https://grpc.testnet.penumbra.zone")]
    node: url::Url,
    /// Bind the gRPC server to this socket.
    ///
    /// The gRPC server supports both grpc (HTTP/2) and grpc-web (HTTP/1.1) clients.
    ///
    /// If `grpc_auto_https` is set, this defaults to `0.0.0.0:443` and uses HTTPS.
    ///
    /// If `grpc_auto_https` is not set, this defaults to `127.0.0.1:8080` without HTTPS.
    #[clap(short, long)]
    grpc_bind: Option<SocketAddr>,
    /// If set, serve gRPC using auto-managed HTTPS with this domain name.
    ///
    /// NOTE: This option automatically provisions TLS certificates from Let's Encrypt and caches
    /// them in the `home` directory.  The production LE CA has rate limits, so be careful using
    /// this option. Avoid deleting the certificates and forcing re-issuance, which can lead to
    /// hitting the rate limit. See the `--acme-staging` option.
    #[clap(long, value_name = "DOMAIN")]
    grpc_auto_https: Option<String>,
    /// Enable use of the LetsEncrypt ACME staging API (https://letsencrypt.org/docs/staging-environment/),
    /// which is more forgiving of ratelimits. Set this option to `true` if you're trying out the
    /// `--grpc-auto-https` option for the first time, to validate your configuration, before
    /// subjecting yourself to production ratelimits.
    ///
    /// This option has no effect if `--grpc-auto-https` is not set.
    #[clap(long)]
    acme_staging: bool,
    // Disable transaction sending. Useful for debugging.
    //  TODO(kate): wire in dry-run mode.
    //  #[clap(long)]
    //  dry_run: bool,
    /// The amounts to send for each response, written as typed values 1.87penumbra, 12cubes, etc.
    values: Vec<Value>,
}

impl ServeRpc {
    /// Run the bot, listening for RPC calls, and responding as appropriate.
    ///
    /// This function should never return, unless an error of some kind is encountered.
    pub async fn exec(self) -> anyhow::Result<()> {
        self.preflight_checks()
            .await
            .context("failed preflight checks")?;

        let Self {
            account_count,
            data_dir,
            node,
            grpc_bind,
            grpc_auto_https,
            acme_staging,
            values,
        } = self;

        // Use the given `grpc_bind` address if one was specified. If not, we will choose a
        // default depending on whether or not `grpc_auto_https` was set. See the
        // `RootCommand::Start::grpc_bind` documentation above.
        let grpc_bind = {
            use std::net::{IpAddr, Ipv4Addr, SocketAddr};
            const HTTP_DEFAULT: SocketAddr =
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)), 8080);
            const HTTPS_DEFAULT: SocketAddr =
                SocketAddr::new(IpAddr::V4(Ipv4Addr::new(0, 0, 0, 0)), 443);
            let default = || {
                if grpc_auto_https.is_some() {
                    HTTPS_DEFAULT
                } else {
                    HTTP_DEFAULT
                }
            };
            grpc_bind.unwrap_or_else(default)
        };

        // Make a worker to handle the address queue.
        let service =
            crate::sender::service::init(data_dir.as_deref(), account_count, &node).await?;
        let (request_tx, responder) = Responder::new(service, 1, values.clone());
        let (cancel_tx, mut cancel_rx) = tokio::sync::mpsc::channel(1);
        let responder = tokio::spawn(async move { responder.run(cancel_tx).await });

        // Next, create the RPC service.
        let make_svc = crate::rpc::rpc(request_tx)?
            .into_router()
            // Set rather permissive CORS headers for pd's gRPC: the service
            // should be accessible from arbitrary web contexts, such as localhost,
            // or any FQDN that wants to reference its data.
            .layer(tower_http::cors::CorsLayer::permissive())
            .into_make_service();

        // Now start the GRPC server, initializing an ACME client to use as a certificate
        // resolver if auto-https has been enabled.
        macro_rules! spawn_grpc_server {
            ($server:expr) => {
                tokio::task::spawn($server.serve(make_svc))
            };
        }
        let grpc_server = axum_server::bind(grpc_bind);
        let grpc_server = match grpc_auto_https {
            // Auto-https is enabled. Configure the axum accepter, and spawn an ACME worker.
            Some(domain) => {
                let data_dir = // we wouldn't need an error here if we hoist the default up.
                    data_dir.ok_or(anyhow!("data directory must be set to use auto-https"))?;
                let (acceptor, acme_worker) =
                    penumbra_auto_https::axum_acceptor(data_dir, domain, !acme_staging);
                // TODO(kate): we should eventually propagate errors from the ACME worker task.
                tokio::spawn(acme_worker);
                spawn_grpc_server!(grpc_server.acceptor(acceptor))
            }
            // Auto-https is not enabled. Spawn only the GRPC server.
            None => {
                spawn_grpc_server!(grpc_server)
            }
        }
        .tap(|_| tracing::info!(address = %grpc_bind, "grpc service is running"));

        // Start the RPC server and the responder worker, listening for a cancellation event.
        tokio::select! {
            result = responder   => result.map_err(anyhow::Error::from)?.context("error in responder service"),
            result = grpc_server => result.map_err(anyhow::Error::from)?.context("error in grpc service"),
            _ = cancel_rx.recv() => Err(anyhow::anyhow!("cancellation received")),
        }
    }

    /// Perform sanity checks on CLI args prior to running.
    async fn preflight_checks(&self) -> anyhow::Result<()> {
        use num_traits::identities::Zero;

        // TODO(kate): wire up dry-run mode.
        // if self.dry_run {
        //     tracing::info!("dry-run mode is enabled, won't send transactions or post messages");
        // }

        if self.values.is_empty() {
            anyhow::bail!("at least one value must be provided");
        } else if self.values.iter().any(|v| v.amount.value().is_zero()) {
            anyhow::bail!("all values must be non-zero");
        }

        Ok(())
    }
}
