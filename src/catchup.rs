use std::fmt::Write;
use std::{collections::HashSet, sync::Arc};

use async_stream::try_stream;
use futures::{Stream, StreamExt};
use serenity::{
    http::Http,
    model::id::{ChannelId, MessageId, UserId},
};
use tokio::sync::mpsc;
use tracing::instrument;

use crate::{
    gather_history,
    responder::{Request, Response},
};

pub struct Catchup {
    /// The channel id to process.
    channel_id: ChannelId,
    /// How many result to report per notification message.
    response_batch_size: usize,
    /// The Discord http context.
    http: Arc<Http>,
    /// The queue of requests to process.
    requests: mpsc::Sender<Request>,
}

impl Catchup {
    pub fn new(
        channel_id: ChannelId,
        response_batch_size: usize,
        http: Arc<Http>,
        requests: mpsc::Sender<Request>,
    ) -> Self {
        Catchup {
            channel_id,
            response_batch_size,
            http,
            requests,
        }
    }

    pub async fn run(self, start_message_id: MessageId) -> anyhow::Result<()> {
        let results = self.gather(start_message_id).await?;
        self.summarize(results).await
    }

    async fn summarize(
        &self,
        mut results: impl Stream<Item = anyhow::Result<(UserId, Response)>> + Send + Unpin + 'static,
    ) -> anyhow::Result<()> {
        fn notification(batch: &mut Vec<(UserId, Response)>) -> String {
            use serenity::prelude::Mentionable;

            let mut notification = "Catching up on backlog: ".to_string();
            for (user_id, _) in batch.drain(..).filter(|(user_id, response)| {
                let keep = !response.complete_failure();
                if !keep {
                    tracing::error!(?user_id, ?response, "failed to send tokens");
                }
                keep
            }) {
                write!(notification, "{} ", user_id.mention()).unwrap();
            }
            notification += "should all have tokens now!";
            notification
        }

        let mut response_batch = Vec::with_capacity(self.response_batch_size);
        while let Some(result) = results.next().await {
            let (user_id, response) = result?;
            response_batch.push((user_id, response));
            if response_batch.len() >= self.response_batch_size {
                let notification = notification(&mut response_batch);
                self.channel_id
                    .send_message(self.http.as_ref(), |m| m.content(notification))
                    .await?;
            }
        }
        if !response_batch.is_empty() {
            let notification = notification(&mut response_batch);
            self.channel_id
                .send_message(self.http.as_ref(), |m| m.content(notification))
                .await?;
        }

        Ok(())
    }

    #[instrument(skip(self))]
    async fn gather(
        &self,
        start: MessageId,
    ) -> anyhow::Result<
        impl Stream<Item = anyhow::Result<(UserId, Response)>> + Send + Unpin + 'static,
    > {
        let requests = self.requests.clone();
        let mut users: HashSet<UserId> = HashSet::new();
        let mut history = gather_history(self.http.clone(), self.channel_id, None, Some(start));

        tracing::info!("gathering history to catch up on...");
        let mut stack = Vec::new();
        while let Some(result) = history.next().await {
            let (_, user, _, response, request) = result?;
            if !users.contains(&user.id) {
                tracing::debug!(user_name = ?user.name, user_id = ?user.id, "adding request to backlog stack");
                users.insert(user.id);
                stack.push((user.id, response, request));
            } else {
                tracing::debug!(user_name = ?user.name, user_id = ?user.id, "duplicate request in backlog");
            }
        }

        Ok(Box::pin(try_stream! {
            tracing::info!("submitting backlog to be processed");
            while let Some((user_id, response, request)) = stack.pop() {
                tracing::debug!(?user_id, "requesting tokens for backlog");
                requests.send(request).await?;
                let response = response.await?;
                yield (user_id, response);
            }
        }))
    }
}
