#![recursion_limit = "256"]
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
    tracing_subscriber::fmt::init();

    use clap::Parser;
    Opt::parse().exec().await
}
