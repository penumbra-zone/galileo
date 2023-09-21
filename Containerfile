ARG PENUMBRA_VERSION=main
# ARG PENUMBRA_VERSION=v0.54.1
# Pull from Penumbra container, so we can grab a recent `pcli` without
# needing to compile from source.
FROM ghcr.io/penumbra-zone/penumbra:${PENUMBRA_VERSION} AS penumbra

# Build the galileo binary
FROM docker.io/rust:1-bookworm AS builder
ARG PENUMBRA_VERSION=main
RUN apt-get update && apt-get install -y \
        libssl-dev git-lfs clang
# Clone in Penumbra deps to relative path, required due to git-lfs.
RUN git clone --depth 1 --branch "${PENUMBRA_VERSION}" https://github.com/penumbra-zone/penumbra /usr/src/penumbra
COPY . /usr/src/galileo
WORKDIR /usr/src/galileo
RUN cargo build --release

# Runtime container, copying in built artifacts
FROM docker.io/debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates
RUN groupadd --gid 1000 penumbra \
        && useradd -m -d /home/penumbra -g 1000 -u 1000 penumbra
COPY --from=penumbra /bin/pcli /usr/bin/pcli
COPY --from=builder /usr/src/galileo/target/release/galileo /usr/bin/galileo
WORKDIR /home/penumbra
USER penumbra
