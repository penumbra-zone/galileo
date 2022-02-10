mod action;
pub use action::ActionQueue;

mod handler;
pub use handler::Handler;

mod responder;
pub use responder::Responder;

mod opt;
pub use opt::Opt;

mod wallet;
pub use wallet::Wallet;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    use clap::Parser;
    Opt::parse().exec().await
}
