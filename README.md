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

This will dispense 100 penumbra tokens and 1000 test\_usd tokens to each address posted in any server to which the bot is
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

## 🔭 Running as a gRPC service

Galileo can be run as a gRPC service, using the `serve-rpc` command. This endpoint supports
reflection, and automatic HTTPS.

```sh
# As galileo, run the RPC service, over plaintext on `localhost:8080`.
cargo run --release -- serve-rpc --account-count=1 1penumbra
```

`grpcurl` can be used to inspect what RPCs are available:

```sh
; grpcurl -plaintext localhost:8080 list
```

A request that the faucet send funds to your address can be made using `grpcurl`, like so:

```sh
# Run `pcli` to acquire a wallet address.
; ADDRESS=$(PCLI_UNLEASH_DANGER=1 pcli view address)

# Call the RPC over plaintext, providing the address:
; grpcurl -plaintext -d @ localhost:8080 galileo.faucet.v1.FaucetService.SendFunds <<EOM
{
    "address": {
        "alt_bech32m": "$ADDRESS"
    }
}
EOM
```

Alternatively, Rust programs use the facilities in `galileo::proto` to construct a client that can
send requests to the faucet service.

Use `--grpc-auto-https` to configure an HTTPS domain, provisioning certificates via LetsEncrypt.
When configuring TLS certificates, it may be wise to use the `--acme-staging` option, to prevent
being rate-limited by the LetsEncrypt API.

❗ **NOTE:** be careful not to expose this RPC on the public internet without proper
authentication.

## Re-deploying after a testnet release
When we deploy a new testnet, we must bounce Galileo to keep the faucet working.
The steps are:

1. Rebuild the Galileo container via [GHA](https://github.com/penumbra-zone/galileo/actions),
   passing in the Penumbra tag version to build from, e.g. `v0.58.0`.
2. Wait for the container build to complete, then run:
   `kubectl set image deployments -l app.kubernetes.io/instance=galileo galileo=penumbra-v0.58.0`
   substituting the correct version in the tag name.

Eventually we should automate these steps so they're performed automatically as part of a release.
If you need to use the `--catch-up` flag, to dispense to prior messages in the Discord channel,
consider running that action locally as a one-off. We don't want to set the catch-up CLI arg
on the primary deployment, where service restarts will cause the catch-up logic to run again.
