# Multi-stage build for ardur-server.
#
# Builder: rust:1.98.1-slim currently tracks Debian 13/trixie, matching the
# distroless runtime base below. Both base images are pinned to manifest-list
# digests so CI/release builds do not silently float to new base contents.
# The builder tag carries the FULL patch version and must equal the
# rust-toolchain.toml channel: a `1.98-slim`-style minor tag floats to whatever
# patch release is current (it resolved to rustc 1.98.0 while the toolchain
# pinned 1.98.1), so CI-validated artifacts and the released image were built by
# different compilers. tests/test_github_security_workflows.py enforces the
# match.
# Runtime: distroless cc-debian13 nonroot. The healthcheck is a small Rust
# binary, so the runtime image does not need curl/wget/shell packages.
# ARD-303: Docker build is validated in CI with a /healthz smoke test.

FROM rust:1.98.1-slim@sha256:ce84a5edd80c5f91e05c5533b1e53eb1da54028f33734dc06aa6b49fa190462d AS builder

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
