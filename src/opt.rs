use std::str::FromStr;

use clap::Parser;
use serenity::model::id::{ChannelId, MessageId};

mod history;
mod serve;

pub use history::gather as gather_history;

#[derive(Debug, Clone, Parser)]
#[clap(author, version, about)]
pub struct Opt {
    #[clap(subcommand)]
    pub command: Command,
}

impl Opt {
    pub async fn exec(self) -> anyhow::Result<()> {
        match self.command {
            Command::Serve(serve) => serve.exec().await,
            Command::History(history) => history.exec().await,
        }
    }
}

#[derive(Debug, Clone, Parser)]
pub enum Command {
    /// Run the bot.
    Serve(serve::Serve),
    /// Export the history of requests from the channel as CSV to stdout.
    History(history::History),
}

/// A pair of channel id and message id that uniquely identifies a message.
#[derive(Debug, Clone)]
pub struct ChannelIdAndMessageId {
    channel_id: ChannelId,
    message_id: MessageId,
}

impl FromStr for ChannelIdAndMessageId {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let parts: Vec<&str> = s.split('/').collect();
        match parts.as_slice() {
            [.., channel_id, message_id] => Ok(ChannelIdAndMessageId {
                channel_id: channel_id.parse()?,
                message_id: message_id.parse::<u64>()?.into(),
            }),
            _ => Err(anyhow::anyhow!("invalid channel/message id: {}", s)),
        }
    }
}
