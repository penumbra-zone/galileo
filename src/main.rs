use serenity::prelude::*;
use std::{env, time::Duration};
use tokio::sync::mpsc;

mod action;
use action::ActionQueue;

mod handler;
use handler::Handler;

mod worker;
use worker::Worker;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    let discord_token = env::var("DISCORD_TOKEN")?;

    let handler = Handler::new(Duration::from_secs(60), vec!["bot-stuff".to_string()]);

    // Make a new client using a token set by an environment variable, with our handlers
    let mut client = Client::builder(&discord_token)
        .event_handler(handler)
        .await?;

    // Put the sending end of the address queue into the global TypeMap
    let (send_actions, receive_actions) = mpsc::channel(100);
    client
        .data
        .write()
        .await
        .insert::<ActionQueue>(send_actions);

    // Get the cache and http part of the client, for use in dispatching replies
    let cache_http = client.cache_and_http.clone();

    // Spawn a task to handle the address queue
    let worker = Worker::new(1, receive_actions, cache_http);

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
