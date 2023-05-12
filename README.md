# Galileo: a Discord bot for the Penumbra discord server 🛰

Galileo runs in the Penumbra discord server and dispenses tokens to any Penumbra address which is
posted in any channel to which it has the send-messages permission.

It does not duplicate command-line wallet management; rather, it shares a wallet by default with the
location of the wallet managed by the `pcli` command line Penumbra wallet. To set up Galileo, first
create a wallet with `pcli`, then send some tokens to that wallet on the test network. Then, you can
run Galileo:

## Obtaining dependencies

You must clone the [penumbra repo](https://github.com/penumbra-zone/penumbra)
side-by-side with the Galileo repo, so that the dependencies are available
as a relative path. This is a temporary workaround to support Git LFS
in the Penumbra dependencies.
See [GH29](https://github.com/penumbra-zone/galileo/issues/29) for details.

## Running it

```bash
RUST_LOG=galileo=info DISCORD_TOKEN=<YOUR DISCORD TOKEN HERE> cargo run --release serve 100penumbra 1000test_usd
```

This will dispense 100 penumbra tokens and 1000 test_usd tokens to each address posted in any server to which the bot is
joined. Note that you must specify the bot's Discord API token using the `DISCORD_TOKEN` environment
variable in order to authenticate with Discord.

On first synchronization, the wallet must be caught up to speed with the state of the chain, which
can take some time; the `info`-level log output will inform you when the bot is ready. In the
meantime, it captures all the addresses which it observes, and holds them in memory until it's ready
to dispense tokens after completing first synchronization.

A variety of options are available, including adjusting rate-limiting, synchronization and
checkpointing intervals, and changing which node to connect to (the default is the hosted Penumbra
default testnet). Use the `--help` option for more details.

## Updating historical testnet allocations
Users of the testnet can post a wallet address to the `#testnet-faucet` channel, and Galileo will
will give them a few funds. We ratelimit those requests to once per day per Discord user.
Since we destroy chain state between testnets (as of 2022Q4), users would lose funds if we don't
manually carry over allocations between testnets. Galileo can help with that!

The following steps assume that the
[penumbra repo](https://github.com/penumbra-zone/penumbra) and the galileo repo are sitting
side-by-side in the filesystem. To update the allocations:

```
# Switch to galileo repo, run history crawl.
cd galileo/
cargo run --release -- history \
          --channel https://discord.com/channels/824484045370818580/915710851917439060 \
          > ../penumbra/testnets/discord_history.csv`

# Switch to penumbra repo.
cd ../penumbra/testnets/
# Generate new testnet directory, including most recent allocations scraped from Discord.
./new-testnet.sh
# Commit, push, and open a PR into the 'penumbra' repo.
```

We perform these updates on best-effort basis, so it's OK if the updates lag somewhat.
Ideally we'd automate the process (https://github.com/penumbra-zone/galileo/issues/17),
but until then, we'll perform it manually, say, every other testnet.

## Re-deploying after a testnet release
When we deploy a new testnet, we must bounce Galileo to keep the faucet working.
The steps are:

```
# Log into galileo host
ssh <your_first_name>@galileo.penumbra.zone
# Stop running service
sudo systemctl stop galileo

# Switch to unprivileged user
sudo su -l www-data -s /bin/bash

# Reset the client state for the new testnet
cd ~/penumbra
git fetch --tags
git checkout <latest_tag>
cargo run --release --bin pcli view reset

# Update galileo's source code
cd ~/galileo
git pull origin main
cargo update
cargo build --release

# Return to normal user
exit

# Edit the catch-up url arg
sudo vim /etc/systemd/system/galileo.service
# Start Galileo again:
sudo systemctl daemon-reload
sudo systemctl restart galileo

# Confirm that Galileo is dispensing tokens by testing the faucet channel with your own address
# Resupply Galileo wallet as needed
# View logs for galileo at any time with:
sudo journalctl -af -u galileo
```

These steps should be performed on release day, immediately after publishing the tag.
