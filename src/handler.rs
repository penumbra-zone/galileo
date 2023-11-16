use std::{
    borrow::Borrow,
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use serenity::{
    async_trait,
    client::{Context, EventHandler},
    model::gateway::Ready,
    model::{
        channel::{GuildChannel, Message},
        id::{GuildId, UserId},
    },
};
use tokio::time::{Duration, Instant};
use tracing::instrument;

use crate::responder::AddressOrAlmost;

use super::responder::{Request, RequestQueue};

/// Represents configuration and state for faucet bot.
pub struct Handler {
    /// The minimum duration between dispensing tokens to a user.
    rate_limit: Duration,
    /// Limit of the number of times, per user, we will inform that user of their rate limit.
    reply_limit: usize,
    /// History of requests we answered for token dispersal, with a timestamp and the number of
    /// times we've told the user about the rate limit (so that eventually we can stop replying if
    /// they keep asking).
    send_history: Arc<Mutex<SendHistory>>,
}

impl Handler {
    pub fn new(rate_limit: Duration, reply_limit: usize) -> Self {
        Handler {
            rate_limit,
            reply_limit,
            send_history: Arc::new(Mutex::new(SendHistory::new())),
        }
    }
    /// Check whether the bot can proceed with honoring this request,
    /// performing preflight checks on server and channel configuration,
    /// avoiding self-sends, and honoring ratelimits.
    async fn should_send(&self, ctx: Context, message: Message) -> anyhow::Result<bool> {
        tracing::trace!("parsing message: {:#?}", message);
        let message_info = MessageInfo::new(message.clone(), ctx.clone())?;
        let self_id = ctx.cache.current_user().id;

        // Stop if we're not allowed to respond in this channel
        if let Ok(self_permissions) = message_info
            .guild_channel
            .permissions_for_user(&ctx, self_id)
        {
            if !self_permissions.send_messages() {
                tracing::debug!(
                    ?message_info.guild_channel,
                    "not allowed to send messages in this channel"
                );
                return Ok(false);
            }
        } else {
            return Ok(false);
        };

        // TODO: add check for channel id, bailing out if channel doesn't match

        // Don't trigger on messages we ourselves send
        if message_info.user_id == self_id {
            tracing::trace!("detected message from ourselves");
            return Ok(false);
        }

        // If we made it this far, it's OK to send.
        return Ok(true);
    }
}

/// Representation of a Discord message, containing only the fields
/// relevant for processing a faucet request.
// TODO: consider using `serenity::model::channel::Message` instead.
pub struct MessageInfo {
    /// Discord user ID of the sender of the message.
    pub user_id: UserId,
    /// Discord human-readable username for sender of message.
    pub user_name: String,
    /// Discord guild (server) ID for the channel in which this message was sent.
    pub guild_id: GuildId,
    /// Discord guild channel in which the message was posted.
    pub guild_channel: GuildChannel,
}

impl MessageInfo {
    pub fn new(message: Message, ctx: Context) -> anyhow::Result<MessageInfo> {
        // Get the guild id of this message. In Discord nomenclature,
        // a "guild" is the overarching server that contains channels.
        let guild_id = match message.guild_id {
            Some(guild_id) => guild_id,
            None => {
                // Assuming Galileo is targeting the Penumbra Labs Discord server,
                // we should never hit this branch.
                tracing::warn!("could not determine guild for message: {:#?}", message);
                anyhow::bail!("");
            }
        };

        // Get the channel of this message.
        let guild_channel = match ctx.cache.guild_channel(message.channel_id) {
            Some(guild_channel) => guild_channel,
            None => {
                // As above, assuming Galileo is targeting the Penumbra Labs Discord server,
                // we should never hit this branch.
                tracing::warn!("could not determine channel for message: {:#?}", message);
                anyhow::bail!("");
            }
        };

        let user_id = message.author.id;
        let user_name = message.author.name.clone();
        Ok(MessageInfo {
            user_id,
            user_name,
            guild_id,
            guild_channel,
        })
    }
}

struct SendHistory {
    pub discord_users: VecDeque<(UserId, Instant, usize)>,
    pub penumbra_addresses: VecDeque<(AddressOrAlmost, Instant, usize)>,
}

impl SendHistory {
    pub fn new() -> Self {
        SendHistory {
            discord_users: VecDeque::new(),
            penumbra_addresses: VecDeque::new(),
        }
    }

    /// Returns whether the given user is rate limited, and if so, when the rate limit will expire along with
    /// the number of times the user has been notified of the rate limit.
    pub fn is_rate_limited(
        &mut self,
        user_id: UserId,
        addresses: &[AddressOrAlmost],
    ) -> std::option::Option<(tokio::time::Instant, usize)> {
        let discord_limited = self
            .discord_users
            .iter_mut()
            .find(|(user, _, _)| *user == user_id)
            .map(|(_, last_fulfilled, notified)| {
                // Increase the notification count by one and return the previous count:
                let old_notified = *notified;
                *notified += 1;
                (*last_fulfilled, old_notified)
            });

        if discord_limited.is_some() {
            return discord_limited;
        }

        self.penumbra_addresses
            .iter_mut()
            .find(|(address_or_almost, _, _)| addresses.contains(address_or_almost))
            .map(|(_, last_fulfilled, notified)| {
                // Increase the notification count by one and return the previous count:
                let old_notified = *notified;
                *notified += 1;
                (*last_fulfilled, old_notified)
            })
    }

    pub fn record_request(&mut self, user_id: UserId, addresses: &[AddressOrAlmost]) {
        self.discord_users.push_back((user_id, Instant::now(), 1));
        self.penumbra_addresses
            .extend(addresses.iter().map(|address_or_almost| {
                (
                    address_or_almost.clone(),
                    Instant::now(),
                    1, // notification count
                )
            }));
    }

    pub fn prune(&mut self, rate_limit: Duration) {
        tracing::trace!("pruning discord user send history");
        while let Some((user, last_fulfilled, _)) = self.discord_users.front() {
            if last_fulfilled.elapsed() >= rate_limit {
                tracing::debug!(?user, ?last_fulfilled, "rate limit expired");
                self.discord_users.pop_front();
            } else {
                break;
            }
        }
        tracing::trace!("finished pruning discord user send history");

        tracing::trace!("pruning penumbra address send history");
        while let Some((address_or_almost, last_fulfilled, _)) = self.penumbra_addresses.front() {
            if last_fulfilled.elapsed() >= rate_limit {
                tracing::debug!(?address_or_almost, ?last_fulfilled, "rate limit expired");
                self.penumbra_addresses.pop_front();
            } else {
                break;
            }
        }
        tracing::trace!("finished pruning penumbra address send history");
    }

    pub fn record_failure(&mut self, user_id: UserId, addresses: &[AddressOrAlmost]) {
        // If the request failed, we set the notification count to zero, so that the rate
        // limit will not apply to future requests
        if let Some((_, _, notified)) = self
            .discord_users
            .iter_mut()
            .find(|(user, _, _)| *user == user_id)
        {
            *notified = notified.saturating_sub(1);
        }
        if let Some((_, _, notified)) = self
            .penumbra_addresses
            .iter_mut()
            .find(|(address_or_almost, _, _)| addresses.contains(address_or_almost))
        {
            *notified = notified.saturating_sub(1);
        }
    }
}

#[async_trait]
impl EventHandler for Handler {
    #[instrument(skip(self, ctx))]
    /// Respond to faucet request. For first-time requests, will spend a bit.
    /// If the faucet request duplicates a previous request within the timeout
    /// window, then Galileo will respond instructing the user to wait longer.
    async fn message(&self, ctx: Context, message: Message) {
        // Check whether we should proceed.
        if let Err(_e) = self.should_send(ctx.clone(), message.clone()).await {
            return;
        }

        let message_info = MessageInfo::new(message.clone(), ctx.clone()).unwrap();

        // Prune the send history of all expired rate limit timeouts
        self.send_history.lock().unwrap().prune(self.rate_limit);

        // Check if the message contains a penumbra address and create a request for it if so
        let (response, request) = if let Some(parsed) = { Request::try_new(&message) } {
            parsed
        } else {
            tracing::trace!("no addresses found in message");
            return;
        };

        let penumbra_addresses = request.addresses().to_owned();

        // If the message author was in the send history, don't send them tokens
        let rate_limited = self
            .send_history
            .lock()
            .unwrap()
            .is_rate_limited(message_info.user_id, &penumbra_addresses);

        if let Some((last_fulfilled, notified)) = rate_limited {
            tracing::info!(
                ?message_info.user_name,
                ?notified,
                user_id = ?message_info.user_id.to_string(),
                ?last_fulfilled,
                "rate-limited user"
            );

            // If we already notified the user, don't reply again
            if notified > self.reply_limit + 1 {
                return;
            }

            let response = format!(
                "Please wait for another {} before requesting more tokens. Thanks!",
                format_remaining_time(last_fulfilled, self.rate_limit)
            );
            reply(&ctx, &message, response).await;

            // Setting the notified count to zero "un-rate-limits" an entry, which we do when a
            // request fails, so we don't have to traverse the entire list:
            if notified > 0 {
                // So therefore we only prevent the request when the notification count is greater
                // than zero
                return;
            }
        }

        // Push the user into the send history queue for rate-limiting in the future
        tracing::trace!(user_name = ?message_info.user_name, user_id = ?message_info.user_id.to_string(), "pushing user into send history");
        self.send_history
            .lock()
            .unwrap()
            .record_request(message_info.user_id, &penumbra_addresses);

        // Send the message to the queue, to be processed asynchronously
        tracing::trace!("sending message to worker queue");
        ctx.data
            .read()
            .await
            .get::<RequestQueue>()
            .expect("address queue exists")
            .send(request)
            .await
            .expect("send to queue always succeeds");

        // Broadcast to the channel that we are typing, so users know something is happening
        if let Err(e) = message_info.guild_channel.broadcast_typing(&ctx).await {
            tracing::error!(error = ?e, "failed to broadcast typing");
        }

        // Reply to the user with the response from the responder
        if let Ok(response) = response.await {
            reply(
                &ctx,
                message,
                response.summary(&ctx, message_info.guild_id).await,
            )
            .await;
        } else {
            self.send_history
                .lock()
                .unwrap()
                .record_failure(message_info.user_id, &penumbra_addresses);
        }
    }

    async fn cache_ready(&self, ctx: Context, guilds: Vec<GuildId>) {
        for guild_id in guilds {
            let server_name = guild_id
                .name(&ctx.cache)
                .unwrap_or_else(|| "[unknown]".to_string());
            tracing::info!(
                ?server_name,
                server_id = ?guild_id.to_string(),
                "connected to server"
            );
        }
    }

    async fn ready(&self, _: Context, ready: Ready) {
        tracing::info!(user = ?ready.user.name, "discord client is connected");
    }
}

/// Send Discord message, replying to a prior message from another user.
/// The message is customizable, because the reply may be to inform
/// about ratelimit.
async fn reply(ctx: &Context, message: impl Borrow<Message>, response: impl Borrow<str>) {
    message
        .borrow()
        .reply_ping(ctx.http.clone(), response.borrow())
        .await
        .map(|_| ())
        .unwrap_or_else(|e| tracing::error!(error = ?e, "failed to reply"));
}

fn format_remaining_time(last_fulfilled: Instant, rate_limit: Duration) -> String {
    humantime::Duration::from(rate_limit - last_fulfilled.elapsed())
        .to_string()
        .split(' ')
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
}
