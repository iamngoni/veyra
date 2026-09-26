# syntax=docker/dockerfile:1.7
# Builds the Rust service as a non-root, read-only production container.

FROM rust:1.93.1-bookworm AS builder

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates git pkg-config \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /src
ENV CARGO_NET_GIT_FETCH_WITH_CLI=true

COPY Cargo.toml Cargo.lock rust-toolchain.toml ./
COPY crates ./crates

# The build shares the Docker VM with the live database. A cached target keeps
# rebuilds incremental, and a job cap leaves CPU for PostgreSQL: an uncapped
# cold release build once stalled audit writes for up to 78 s.
ARG BUILD_JOBS=6
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/src/target \
    cargo build --locked --release --jobs "${BUILD_JOBS}" --package veyra-service \
    && install -Dm755 target/release/veyra-service /out/veyra-service

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates curl \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system --gid 10001 veyra \
    && useradd --system --uid 10001 --gid veyra --home-dir /nonexistent --shell /usr/sbin/nologin veyra

COPY --from=builder /out/veyra-service /usr/local/bin/veyra-service

USER veyra
EXPOSE 7801 8080
ENTRYPOINT ["/usr/local/bin/veyra-service"]
