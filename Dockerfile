# Multi-stage build for ardur-server.
#
# Builder: rust:1.96-slim currently tracks Debian 13/trixie, matching the
# distroless runtime base below. Both base images are pinned to manifest-list
# digests so CI/release builds do not silently float to new base contents.
# Runtime: distroless cc-debian13 nonroot. The healthcheck is a small Rust
# binary, so the runtime image does not need curl/wget/shell packages.
# ARD-303: Docker build is validated in CI with a /healthz smoke test.

FROM rust:1.96-slim@sha256:31ee7fc65186be7e0e0ccb3f2ca305f14e4739e7642a1ae65753aa5d7b874523 AS builder

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

FROM gcr.io/distroless/cc-debian13:nonroot@sha256:aded2458d026e046cb68199db0e5793e1028ffa143f7258f3c4278253e20add7

COPY --from=builder /build/target/release/ardur-server /usr/local/bin/ardur-server
COPY --from=builder /build/target/release/ardur-healthcheck /usr/local/bin/ardur-healthcheck
COPY --from=builder --chown=nonroot:nonroot /ardur-data/ /var/lib/ardur/

WORKDIR /var/lib/ardur
EXPOSE 3000

HEALTHCHECK --interval=30s --timeout=3s --start-period=10s --retries=3 \
  CMD ["/usr/local/bin/ardur-healthcheck"]

USER nonroot:nonroot
ENTRYPOINT ["/usr/local/bin/ardur-server"]
