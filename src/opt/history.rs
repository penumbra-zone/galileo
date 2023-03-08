use std::{env, sync::Arc};

use anyhow::Context;
use async_stream::stream;
use clap::Parser;
use csv;
use futures::{Stream, StreamExt};
use serde::Serialize;
use serenity::{
    http::Http,
    model::{
        id::{ChannelId, MessageId, UserId},
        prelude::User,
        Timestamp,
    },
    prelude::GatewayIntents,
    // utils::token,
};
use tokio::sync::oneshot;

use crate::responder::{AddressOrAlmost, Request, Response};

#[derive(Debug, Clone, Parser)]
pub struct History {
    /// The channel from which to export history.
    #[clap(long, short, parse(try_from_str = parse_channel_id))]
    channel: ChannelId,
    /// The last message to include in the export (default: latest).
    #[clap(long, short, parse(try_from_str = parse_message_id))]
    before: Option<MessageId>,
    /// The earliest message to include in the export (default: latest).
    #[clap(long, short, parse(try_from_str = parse_message_id))]
    after: Option<MessageId>,
}

fn parse_message_id(s: &str) -> Result<MessageId, anyhow::Error> {
    let parts: Vec<&str> = s.split('/').collect();
    match parts.as_slice() {
        [.., message_id] => Ok(MessageId(message_id.parse().context("invalid message id")?)),
        _ => Err(anyhow::anyhow!("invalid message id")),
    }
}

fn parse_channel_id(s: &str) -> Result<ChannelId, anyhow::Error> {
    let parts: Vec<&str> = s.split('/').collect();
    match parts.as_slice() {
        [.., channel_id] => Ok(ChannelId(channel_id.parse().context("invalid channel id")?)),
        _ => Err(anyhow::anyhow!("invalid channel id")),
    }
}

impl History {
    pub async fn exec(self) -> anyhow::Result<()> {
        let discord_token =
            env::var("DISCORD_TOKEN").context("missing environment variable DISCORD_TOKEN")?;

        // TODO: i think the serenity token validation logic has a bug somewhere because it always
        // fails with seemingly correct tokens:
        // https://docs.rs/serenity/0.11.5/src/serenity/utils/token.rs.html
        // if token::validate(discord_token.clone()).is_err() {
        //     anyhow::bail!("invalid discord token");
        // }

        // "Default" GateWayIntents are all of them except for the privileged ones.
        let client = serenity::Client::builder(
            &discord_token,
            GatewayIntents::non_privileged() | GatewayIntents::MESSAGE_CONTENT,
        )
        .await?;

        let mut history = gather(
            client.cache_and_http.http.clone(),
            self.channel,
            self.before,
            self.after,
        );

        #[derive(Serialize, Debug)]
        struct Row {
            timestamp: Timestamp,
            user_name: String,
            user_discriminator: u16,
            user_id: UserId,
            message_id: MessageId,
            // Using display impl to get human-readable address for serialization.
            // Type is checked via enum.
            // address: penumbra_crypto::Address,
            address: String,
        }
        let mut wtr = csv::WriterBuilder::new()
            .has_headers(true)
            .from_writer(std::io::stdout());

        while let Some(result) = history.next().await {
            let (timestamp, user, message_id, _, request) = result?;

            // Skip bot messages (that is, ones we sent)
            if user.bot {
                continue;
            }

            for address in request.addresses() {
                if let AddressOrAlmost::Address(address) = address {
                    let row = Row {
                        timestamp,
                        user_name: user.name.clone(),
                        user_discriminator: user.discriminator,
                        user_id: user.id,
                        message_id,
                        // Use display formatting to get a human-readable address
                        address: format!("{}", *address.clone()),
                    };
                    let debug_msg = format!("Failed to serialize row: {:?}", &row);
                    wtr.serialize(row).context(debug_msg)?;
                }
            }
        }

        Ok(())
    }
}

// Gather and parse into requests messages in a given channel, streaming the results in reverse
// chronological order.
pub fn gather(
    http: Arc<Http>,
    channel_id: ChannelId,
    mut before: Option<MessageId>,
    after: Option<MessageId>,
) -> impl Stream<
    Item = anyhow::Result<(
        Timestamp,
        User,
        MessageId,
        oneshot::Receiver<Response>,
        Request,
    )>,
> + Send
       + Unpin
       + 'static {
    Box::pin(stream! {
        let after_message = if let Some(after) = after {
            let message = channel_id.message(http.as_ref(), after).await?;
            if message.channel_id != channel_id {
                yield Err(anyhow::anyhow!("after message is not in the channel"));
                return;
            }
            Some(message)
        } else {
            None
        };

        loop {
            let messages = channel_id.messages(http.as_ref(), |retriever| if let Some(before) = before {
                retriever.before(before)
            } else {
                retriever
            }).await?;
            if messages.is_empty() {
                break;
            }

            for message in messages {
                if let Some((response, request)) = Request::try_new(&message) {
                    yield Ok((message.timestamp, message.author, message.id, response, request));

                    // Terminate after we see the after-message
                    if let Some(ref after_message) = after_message {
                        if message.id == after_message.id {
                            return;
                        }
                    }
                }
                before = Some(message.id);
            }
        }
    })
}
