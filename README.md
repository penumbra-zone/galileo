# Galileo: a Discord bot for the Penumbra discord server 🛰

Galileo runs in the Penumbra discord server and dispenses tokens to any Penumbra address which is
posted in any channel to which it has the send-messages permission.

It does not duplicate command-line wallet management; rather, it shares a wallet by default with the
location of the wallet managed by the `pcli` command line Penumbra wallet. To set up Galileo, first
create a wallet with `pcli`, then send some tokens to that wallet on the test network. Then, you can
run Galileo:

```bash
RUST_LOG=galileo=info DISCORD_TOKEN=<YOUR DISCORD TOKEN HERE> cargo run --release serve 100penumbra
```

This will dispense 100 penumbra tokens to each address posted in any server to which the bot is
joined. Note that you must specify the bot's Discord API token using the `DISCORD_TOKEN` environment
variable in order to authenticate with Discord.

On first synchronization, the wallet must be caught up to speed with the state of the chain, which
can take some time; the `info`-level log output will inform you when the bot is ready. In the
meantime, it captures all the addresses which it observes, and holds them in memory until it's ready
to dispense tokens after completing first synchronization.

A variety of options are available, including adjusting rate-limiting, synchronization and
checkpointing intervals, and changing which node to connect to (the default is the hosted Penumbra
default testnet). Use the `--help` option for more details.
