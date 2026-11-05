# Multi-stage build of chattercrab-signal only. The client and media crates need
# native audio and video libraries and are not part of this image.
# rust-toolchain.toml is intentionally absent: see .dockerignore.

FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates/protocol crates/protocol
COPY crates/server crates/server
# Cargo.lock lists every workspace member. This image only has protocol and
# server, so Cargo must drop the client and media crates from the lockfile
# before --locked will succeed. That refresh keeps the pinned versions of the
# crates the server actually uses.
RUN cargo metadata --format-version 1 >/dev/null \
    && cargo build --locked --release -p chattercrab-server --bin chattercrab-signal

FROM debian:bookworm-slim
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates wget \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --system --uid 10001 --no-create-home --shell /usr/sbin/nologin chattercrab
COPY --from=build /src/target/release/chattercrab-signal /usr/local/bin/chattercrab-signal
USER chattercrab
ENV CHATTERCRAB_BIND=0.0.0.0:8080
EXPOSE 8080
HEALTHCHECK CMD wget -qO- http://127.0.0.1:8080/health || exit 1
ENTRYPOINT ["/usr/local/bin/chattercrab-signal"]
