# Pull from Penumbra container, so we can grab a recent `pcli` without
# needing to compile from source.
FROM ghcr.io/penumbra-zone/penumbra:main AS penumbra
FROM docker.io/rust AS builder

RUN apt-get update && apt-get install -y \
        libssl-dev git-lfs clang
# Shallow clone since we only want most recent HEAD; this should change
# if/when we want to support specific refs, such as release tags, for Penumbra deps.
RUN git clone --depth=1 https://github.com/penumbra-zone/penumbra /app/penumbra
COPY . /app/galileo
WORKDIR /app/galileo
RUN cargo build --release

FROM docker.io/debian:stable-slim
RUN groupadd --gid 1000 penumbra \
        && useradd -m -d /home/penumbra -g 1000 -u 1000 penumbra
COPY --from=builder /app/galileo/target/release/galileo /usr/bin/galileo
COPY --from=penumbra /bin/pcli /usr/bin/pcli
WORKDIR /home/penumbra
USER penumbra
