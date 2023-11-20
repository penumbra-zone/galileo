# run local instance of galileo, in dry-run mode, meaning no funds will be sent.
# assumes you've imported the galileo seed phrase locally
dev:
    cargo run --release -- serve --dry-run 1penumbra --data-dir ~/.local/share/pcli-galileo

# scan discord history for faucet drips, collect in local csv, for inclusion in allocations.
history:
    cargo run --release -- history \
        --channel https://discord.com/channels/824484045370818580/915710851917439060 \
        | tee discord-history.csv
