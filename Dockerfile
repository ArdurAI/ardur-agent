# Multi-stage build for ardur-server.
#
# Builder: rust:1.85-slim matches the workspace MSRV in Cargo.toml.
# Runtime: distroless cc-debian12 nonroot. The healthcheck is a small Rust
# binary, so the runtime image does not need curl/wget/shell packages.

FROM rust:1.85-slim AS builder

# pkg-config + libssl-dev cover openssl-sys transitive dependencies. ca-certificates
# is needed for crates.io fetches over HTTPS during `cargo build`.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

RUN cargo build --release --bin ardur-server --bin ardur-healthcheck

FROM gcr.io/distroless/cc-debian12:nonroot

COPY --from=builder /build/target/release/ardur-server /usr/local/bin/ardur-server
COPY --from=builder /build/target/release/ardur-healthcheck /usr/local/bin/ardur-healthcheck

WORKDIR /var/lib/ardur
EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
  CMD ["/usr/local/bin/ardur-healthcheck"]

USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/ardur-server"]
