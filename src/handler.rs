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
        channel::Message,
        id::{GuildId, UserId},
    },
};
use tokio::time::{Duration, Instant};
use tracing::instrument;

use super::responder::{Request, RequestQueue};

pub struct Handler {
    /// The minimum duration between dispensing tokens to a user.
    rate_limit: Duration,
    /// Limit of the number of times, per user, we will inform that user of their rate limit.
    reply_limit: usize,
    /// History of requests we answered for token dispersal, with a timestamp and the number of
    /// times we've told the user about the rate limit (so that eventually we can stop replying if
    /// they keep asking).
    send_history: Arc<Mutex<VecDeque<(UserId, Instant, usize)>>>,
}

impl Handler {
    pub fn new(rate_limit: Duration, reply_limit: usize) -> Self {
        Handler {
            rate_limit,
            reply_limit,
            send_history: Arc::new(Mutex::new(VecDeque::new())),
        }
    }
}

#[async_trait]
impl EventHandler for Handler {
    #[instrument(skip(self, ctx))]
    async fn message(&self, ctx: Context, message: Message) {
        tracing::trace!("parsing message: {:#?}", message);
        // Get the guild id of this message
        let guild_id = if let Some(guild_id) = message.guild_id {
            guild_id
        } else {
            return;
        };

        // Get the channel of this message
        let guild_channel = if let Some(guild_channel) = ctx.cache.guild_channel(message.channel_id)
        {
            guild_channel
        } else {
            tracing::trace!("could not find server");
            return;
        };

        let self_id = ctx.cache.current_user().id;
        let user_id = message.author.id;
        let user_name = message.author.name.clone();

        // Stop if we're not allowed to respond in this channel
        if let Ok(self_permissions) = guild_channel.permissions_for_user(&ctx, self_id) {
            if !self_permissions.send_messages() {
                tracing::trace!(
                    ?guild_channel,
                    "not allowed to send messages in this channel"
                );
                return;
            }
        } else {
            return;
        };

        // Don't trigger on messages we ourselves send
        if user_id == self_id {
            tracing::trace!("detected message from ourselves");
            return;
        }

        // Prune the send history of all expired rate limit timeouts
        {
            tracing::trace!("pruning send history");
            // scoped to prevent deadlock on send_history
            let mut send_history = self.send_history.lock().unwrap();
            while let Some((user, last_fulfilled, _)) = send_history.front() {
                if last_fulfilled.elapsed() >= self.rate_limit {
                    tracing::debug!(?user, ?last_fulfilled, "rate limit expired");
                    send_history.pop_front();
                } else {
                    break;
                }
            }
            tracing::trace!("finished pruning send history");
        }

        // Check if the message contains a penumbra address and create a request for it if so
        let (response, request) = if let Some(parsed) = { Request::try_new(&message) } {
            parsed
        } else {
            tracing::trace!("no addresses found in message");
            return;
        };

        // If the message author was in the send history, don't send them tokens
        let rate_limited = self
            .send_history
            .lock()
            .unwrap()
            .iter_mut()
            .find(|(user, _, _)| *user == user_id)
            .map(|(_, last_fulfilled, notified)| {
                // Increase the notification count by one and return the previous count:
                let old_notified = *notified;
                *notified += 1;
                (*last_fulfilled, old_notified)
            });

        if let Some((last_fulfilled, notified)) = rate_limited {
            tracing::info!(
                ?user_name,
                ?notified,
                user_id = ?user_id.to_string(),
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
        tracing::trace!(?user_name, user_id = ?user_id.to_string(), "pushing user into send history");
        self.send_history
            .lock()
            .unwrap()
            .push_back((user_id, Instant::now(), 1));

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
        if let Err(e) = guild_channel.broadcast_typing(&ctx).await {
            tracing::error!(error = ?e, "failed to broadcast typing");
        }

        // Reply to the user with the response from the responder
        if let Ok(response) = response.await {
            reply(&ctx, message, response.summary(&ctx, guild_id).await).await;
        } else {
            self.send_history
                .lock()
                .unwrap()
                .iter_mut()
                .find(|(user, _, _)| *user == user_id)
                .map(|(_, _, notified)| {
                    // If the request failed, we set the notification count to zero, so that the rate
                    // limit will not apply to future requests
                    *notified = notified.saturating_sub(1);
                });
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
        tracing::info!("{} is connected!", ready.user.name);
    }
}

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
