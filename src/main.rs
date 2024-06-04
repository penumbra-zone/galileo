#![recursion_limit = "256"]

mod handler;
pub use handler::Handler;

mod responder;
pub use responder::Responder;

mod sender;
pub use sender::Sender;

mod opt;
pub use opt::{gather_history, Opt};

mod tracing;

mod wallet;
pub use wallet::Wallet;

mod catchup;
pub use catchup::Catchup;

#[cfg(feature = "rpc")]
pub mod proto;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Configuring logging
    self::tracing::init_subscriber()?;

    use clap::Parser;
    Opt::parse().exec().await
}
