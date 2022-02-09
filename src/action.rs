use serenity::{model::channel::Message, prelude::TypeMapKey};
use tokio::{
    sync::mpsc,
    time::{Duration, Instant},
};

/// `TypeMap` key for the address queue.
pub struct ActionQueue;

/// Associate the `AddressQueue` key with an `mpsc::Sender` for `AddressQueueMessage`s in the `TypeMap`.
impl TypeMapKey for ActionQueue {
    type Value = mpsc::Sender<Action>;
}

/// `TypeMap` value for the sender end of the address queue.
#[derive(Debug, Clone)]
pub enum Action {
    Dispense {
        /// The originating message that contained these addresses.
        message: Message,
        /// The addresses matched in the originating message.
        addresses: Vec<String>,
    },
    RateLimit {
        /// The originating message that resulted in this rate-limit.
        message: Message,
        /// The last time this request was fulfilled for the user.
        last_fulfilled: Instant,
        /// The rate limit duration.
        rate_limit: Duration,
    },
}
