use std::{collections::HashSet, sync::Arc};

use async_stream::try_stream;
use futures::{Stream, StreamExt};
use serenity::{
    http::Http,
    model::id::{ChannelId, MessageId, UserId},
};
use tokio::sync::mpsc;

use crate::responder::{Request, Response};

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
        let results = self.gather(start_message_id);
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
                    tracing::debug!(?user_id, ?response, "failed to send tokens");
                }
                keep
            }) {
                notification.push_str(&format!("{} ", user_id.mention()));
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

    fn gather(
        &self,
        mut start_message_id: MessageId,
    ) -> impl Stream<Item = anyhow::Result<(UserId, Response)>> + Send + Unpin + 'static {
        let requests = self.requests.clone();
        let http = self.http.clone();
        let channel_id = self.channel_id;

        let mut users: HashSet<UserId> = HashSet::new();
        Box::pin(try_stream! {
            loop {
                let messages = channel_id.messages(http.as_ref(), |retriever| retriever.after(start_message_id)).await?;
                if messages.is_empty() {
                    break;
                }

                for message in messages {
                    if !users.contains(&message.author.id) {
                        users.insert(message.author.id);
                        if let Some((response, request)) = Request::try_new(&message) {
                            tracing::debug!(user_id = ?message.author.id, "performing catch-up distribution");
                            requests.send(request).await?;
                            let response = response.await?;
                            yield (message.author.id, response);
                        }
                    }
                    start_message_id = message.id;
                }
            }
        })
    }
}
