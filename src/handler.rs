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
        user::User,
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
    /// Whether read-only mode is enabled. If false, Penumbra transactions will be sent,
    /// and Discord replies will be posted. If true, those events will be logged, but not actually
    /// performed.
    dry_run: bool,
}

impl Handler {
    /// Constructs a new [`Handler`].
    pub fn new(rate_limit: Duration, reply_limit: usize, dry_run: bool) -> Self {
        Handler {
            rate_limit,
            reply_limit,
            send_history: Arc::new(Mutex::new(SendHistory::new())),
            dry_run,
        }
    }

    /// Check whether the bot can proceed with honoring this request,
    /// performing preflight checks on server and channel configuration,
    /// avoiding self-sends, and honoring ratelimits.
    async fn should_send(
        &self,
        ctx: &Context,
        MessageInfo {
            user_id,
            guild_channel,
            ..
        }: &MessageInfo,
    ) -> bool {
        let self_id = ctx.cache.current_user().id;

        // Stop if we're not allowed to respond in this channel
        if let Ok(self_permissions) = guild_channel.permissions_for_user(&ctx, self_id) {
            if !self_permissions.send_messages() {
                tracing::debug!(
                    ?guild_channel,
                    "not allowed to send messages in this channel"
                );
                return false;
            }
        } else {
            return false;
        };

        // TODO: add check for channel id, bailing out if channel doesn't match

        // Don't trigger on messages we ourselves send
        if *user_id == self_id {
            tracing::trace!("detected message from ourselves");
            return false;
        }

        // If we made it this far, it's OK to send.
        return true;
    }
}

/// Representation of a Discord message, containing only the fields
/// relevant for processing a faucet request.
// TODO: consider using `serenity::model::channel::Message` instead.
#[derive(Debug)]
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
    fn new(
        message @ Message {
            author: User { id, name, .. },
            channel_id,
            guild_id,
            ..
        }: &Message,
        ctx: &Context,
    ) -> anyhow::Result<MessageInfo> {
        // Get the guild id of this message. In Discord nomenclature,
        // a "guild" is the overarching server that contains channels.
        let guild_id = match guild_id {
            Some(guild_id) => guild_id,
            None => {
                // Assuming Galileo is targeting the Penumbra Labs Discord server,
                // we should never hit this branch.
                tracing::warn!("could not determine guild for message: {:#?}", message);
                anyhow::bail!("");
            }
        };

        // Get the channel of this message.
        let guild_channel = match ctx.cache.guild_channel(channel_id) {
            Some(guild_channel) => guild_channel,
            None => {
                // As above, assuming Galileo is targeting the Penumbra Labs Discord server,
                // we should never hit this branch.
                tracing::warn!("could not determine channel for message: {:#?}", message);
                anyhow::bail!("");
            }
        };

        Ok(MessageInfo {
            user_id: *id,
            user_name: name.clone(),
            guild_id: *guild_id,
            guild_channel,
        })
    }
}

struct SendHistory {
    pub discord_users: VecDeque<(UserId, Instant, usize)>,
    pub penumbra_addresses: VecDeque<(AddressOrAlmost, Instant, usize)>,
}

impl SendHistory {
    fn new() -> Self {
        SendHistory {
            discord_users: VecDeque::new(),
            penumbra_addresses: VecDeque::new(),
        }
    }

    /// Returns whether the given user is rate limited, and if so, when the rate limit will expire along with
    /// the number of times the user has been notified of the rate limit.
    fn is_rate_limited(
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

    fn record_request(&mut self, user_id: UserId, addresses: &[AddressOrAlmost]) {
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

    fn prune(&mut self, rate_limit: Duration) {
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

    fn record_failure(&mut self, user_id: UserId, addresses: &[AddressOrAlmost]) {
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
    // The `Message` struct is very large, and clutters up debug logging, so we'll
    // extract specific fields to make spans readable.
    #[instrument(skip_all, fields(
            user_name = %message.author.name,
            user_id = %message.author.id,
            message_id = %message.id,
    ))]
    /// Respond to faucet request. For first-time requests, will spend a bit.
    /// If the faucet request duplicates a previous request within the timeout
    /// window, then Galileo will respond instructing the user to wait longer.
    async fn message(&self, ctx: Context, message: Message) {
        // Check whether we should proceed.
        let message_info = match MessageInfo::new(&message, &ctx) {
            Ok(m) => m,
            Err(e) => {
                // We can't return an error, but we also can't proceed, so log the error
                // and early-return to bail out.
                tracing::error!(%e, "failed to get message info for message");
                return;
            }
        };
        if !self.should_send(&ctx, &message_info).await {
            return;
        }

        // Prune the send history of all expired rate limit timeouts
        self.send_history.lock().unwrap().prune(self.rate_limit);

        // Check if the message contains a penumbra address and create a request for it if so
        let (response, request) = if let Some(parsed) = Request::try_new(&message) {
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
            if !self.dry_run {
                reply(&ctx, &message, response).await;
            } else {
                tracing::debug!("dry_run: would reply with rate-limit response",);
            }

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

        if self.dry_run {
            tracing::debug!("dry_run: would send tx");
            return;
        }

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
