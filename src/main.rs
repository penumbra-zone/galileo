#![recursion_limit = "256"]

use tracing_subscriber::{prelude::*, EnvFilter};

mod handler;
pub use handler::Handler;

mod responder;
pub use responder::Responder;

mod sender;
pub use sender::Sender;

mod opt;
pub use opt::{gather_history, Opt};

mod wallet;
pub use wallet::Wallet;

mod catchup;
pub use catchup::Catchup;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Configuring logging
    let fmt_layer = tracing_subscriber::fmt::layer().with_target(true);
    let filter_layer = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info"))?
        // Force disabling of r1cs log messages, otherwise the `ark-groth16` crate
        // can use a massive (>16GB) amount of memory generating unused trace statements.
        .add_directive("r1cs=off".parse()?);
    let registry = tracing_subscriber::registry()
        .with(filter_layer)
        .with(fmt_layer);
    registry.init();

    use clap::Parser;
    Opt::parse().exec().await
}
