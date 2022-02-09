use clap::Parser;
use std::time::Duration;

#[derive(Debug, Clone, Parser)]
#[clap(author, version, about)]
pub struct Opt {
    /// Per-user rate limit (e.g. "10m" or "1day").
    #[clap(short, long, default_value = "1h", parse(try_from_str = humantime::parse_duration))]
    pub rate_limit: Duration,
    /// Channel name in which to interact (can be specified more than once).
    #[clap(short, long)]
    pub channel: Vec<String>,
    /// Maximum number of addresses per message to which to dispense tokens.
    #[clap(default_value = "1")]
    pub max_addresses: usize,
    /// Internal buffer size for the queue of actions to perform.
    #[clap(default_value = "100")]
    pub buffer_size: usize,
}
