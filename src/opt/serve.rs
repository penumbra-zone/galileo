use clap::Parser;
use std::{env, time::Duration};

use crate::{ActionQueue, Handler, Worker};

#[derive(Debug, Clone, Parser)]
pub struct Serve {
    /// Per-user rate limit (e.g. "10m" or "1day").
    #[clap(short, long, default_value = "1h", parse(try_from_str = humantime::parse_duration))]
    rate_limit: Duration,
    /// Maximum number of times to reply to a user informing them of the rate limit.
    #[clap(long, default_value = "5")]
    reply_limit: usize,
    /// Maximum number of addresses per message to which to dispense tokens.
    #[clap(default_value = "1")]
    max_addresses: usize,
    /// Internal buffer size for the queue of actions to perform.
    #[clap(default_value = "100")]
    buffer_size: usize,
}

impl Serve {
    pub async fn exec(self) -> anyhow::Result<()> {
        let discord_token = env::var("DISCORD_TOKEN")?;

        let handler = Handler::new(self.rate_limit, self.reply_limit);

        // Make a new client using a token set by an environment variable, with our handlers
        let mut client = serenity::Client::builder(&discord_token)
            .event_handler(handler)
            .await?;

        // Get the cache and http part of the client, for use in dispatching replies
        let cache_http = client.cache_and_http.clone();

        // Spawn a task to handle the address queue
        let (send_actions, worker) = Worker::new(self.max_addresses, cache_http, self.buffer_size);

        // Put the sending end of the address queue into the global TypeMap
        client
            .data
            .write()
            .await
            .insert::<ActionQueue>(send_actions);

        // Start the client and the worker
        tokio::select! {
            result = client.start() => if let Err(e) = result {
                tracing::error!(error = ?e, "client error");
            },
            result = tokio::spawn(worker.run()) => if let Err(e) = result {
                tracing::error!(erorr = ?e, "worker error");
            },
        }

        Ok(())
    }
}
