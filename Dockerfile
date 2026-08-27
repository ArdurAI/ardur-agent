# Multi-stage build for ardur-server.
#
# Builder: rust:1.96-slim currently tracks Debian 13/trixie, matching the
# distroless runtime base below. Both base images are pinned to manifest-list
# digests so CI/release builds do not silently float to new base contents.
# Runtime: distroless cc-debian13 nonroot. The healthcheck is a small Rust
# binary, so the runtime image does not need curl/wget/shell packages.
# ARD-303: Docker build is validated in CI with a /healthz smoke test.

FROM rust:1.97-slim@sha256:14c4fe50ea427dc42381a1a09a9a839c1d2346a2e508cd491bf02c659dbc0ed7 AS builder

# pkg-config + libssl-dev cover openssl-sys transitive dependencies. g++ provides
# libstdc++ for native ML/search dependencies at the final link step.
# ca-certificates is needed for crates.io fetches over HTTPS during `cargo build`.
RUN apt-get update \
    && apt-get install -y --no-install-recommends \
        pkg-config \
        libssl-dev \
        g++ \
        ca-certificates \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build
COPY . .

RUN cargo build --release --bin ardur-server --bin ardur-healthcheck
RUN mkdir -p /ardur-data

FROM gcr.io/distroless/cc-debian13:nonroot@sha256:c31ff9abcb1910f3ab25c7957bdaf0bfe12a01eb546e8df2282f1c8f682b606c

COPY --from=builder /build/target/release/ardur-server /usr/local/bin/ardur-server
COPY --from=builder /build/target/release/ardur-healthcheck /usr/local/bin/ardur-healthcheck
COPY --from=builder --chown=nonroot:nonroot /ardur-data/ /var/lib/ardur/

WORKDIR /var/lib/ardur
EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
  CMD ["/usr/local/bin/ardur-healthcheck"]

USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/ardur-server"]
