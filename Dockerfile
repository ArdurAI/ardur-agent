# Multi-stage build for ardur-server.
#
# Builder: rust:1.85-slim (matches workspace `rust-version = "1.85"` in
# Cargo.toml). Runtime: gcr.io/distroless/cc-debian12 — smallest base that
# still ships libc + libgcc, which the default-unwind Rust binary needs.
#
# CI: skip docker build until ardur-server lands (sibling PR pending).
# Once `crates/server/src/bin/ardur-server.rs` is on `dev`, a follow-up PR
# can add a `docker build .` step to .github/workflows/ci.yml.

FROM rust:1.85-slim AS builder

# pkg-config + libssl-dev cover the openssl-sys transitive dep pulled in by
# the HTTP stack (slack-adapter → reqwest). ca-certificates is needed for
# crates.io fetches over HTTPS during `cargo build`.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

COPY . .

RUN cargo build --release --bin ardur-server

FROM gcr.io/distroless/cc-debian12

COPY --from=builder /build/target/release/ardur-server /usr/local/bin/ardur-server

WORKDIR /var/lib/ardur

EXPOSE 3000

USER nonroot

ENTRYPOINT ["/usr/local/bin/ardur-server"]
