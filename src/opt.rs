//! Command-line facilities for running Galileo.
//!
//! See [`Opt`].

pub use self::history::gather as gather_history;

use clap::Parser;
use serenity::model::id::{ChannelId, MessageId};
use std::str::FromStr;

mod history;
mod rpc;
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
            Command::Serve(serve) => serve.exec().await,
            Command::ServeRpc(rpc) => rpc.exec().await,
            Command::History(history) => history.exec().await,
        }
    }
}

#[derive(Debug, Clone, Parser)]
pub enum Command {
    /// Run the bot, listening for Discord messages, dispensing tokens, and replying to users.
    Serve(serve::Serve),
    /// Run the bot, listening for RPC calls, and responding as appropriate.
    ServeRpc(rpc::ServeRpc),
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
