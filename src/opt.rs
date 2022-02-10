use clap::Parser;

mod serve;

#[derive(Debug, Clone, Parser)]
#[clap(author, version, about)]
pub struct Opt {
    #[clap(subcommand)]
    pub command: Command,
}

impl Opt {
    pub async fn exec(self) -> anyhow::Result<()> {
        match self.command {
            Command::Serve(run) => run.exec().await,
        }
    }
}

#[derive(Debug, Clone, Parser)]
pub enum Command {
    /// Run the bot.
    Serve(serve::Serve),
}
