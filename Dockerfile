# syntax=docker/dockerfile:1
ARG RUST_VERSION=1.95.0
FROM rust:${RUST_VERSION}-bookworm AS builder
RUN apt-get update && apt-get install --no-install-recommends -y \
    build-essential cmake nasm pkg-config libdbus-1-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
ARG TARGETARCH
# Run python3 scripts/sdk-prepare.py in the checkout before building. The Rust
# build guard verifies the prepared SDK and patch digests again inside Docker.
COPY Cargo.toml Cargo.lock ./
COPY src/ src/
COPY crates/ crates/
COPY src-tauri/ src-tauri/
COPY sdk/ sdk/
RUN test -f sdk/generated/tuta-sdk/rust/sdk/Cargo.toml || \
    (echo 'Prepare the SDK first: python3 scripts/sdk-prepare.py' >&2; exit 1)
ARG TUTABRIDGE_VERSION
ENV TUTABRIDGE_VERSION=${TUTABRIDGE_VERSION}
ARG CARGO_BUILD_JOBS=3
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,id=tutabridge-target-${TARGETARCH},target=/build/target,sharing=locked \
    cargo build --locked --release --jobs "${CARGO_BUILD_JOBS}" -p tutabridge \
    && cp target/release/tutabridge /build/tutabridge

# Test-only stage, never included in the runtime image.
FROM builder AS test-builder
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,id=tutabridge-target-${TARGETARCH},target=/build/target,sharing=locked \
    cargo test --locked --release --no-run --jobs "${CARGO_BUILD_JOBS}" -p tutabridge-core --lib --message-format=json > /tmp/core.json \
    && cp "$(sed -n 's/.*"executable":"\([^" ]*\)".*/\1/p' /tmp/core.json)" /build/test-core \
    && cargo test --locked --release --no-run --jobs "${CARGO_BUILD_JOBS}" -p tutabridge --bin tutabridge --message-format=json > /tmp/service.json \
    && cp "$(sed -n 's/.*"executable":"\([^" ]*\)".*/\1/p' /tmp/service.json)" /build/test-service
FROM scratch AS test-binaries
COPY --from=test-builder /build/test-core /test-core
COPY --from=test-builder /build/test-service /test-service

FROM debian:bookworm-slim AS runtime
ARG TUTABRIDGE_VERSION
ARG TUTABRIDGE_REVISION
ARG TUTABRIDGE_CREATED
LABEL org.opencontainers.image.source="https://github.com/spartanz51/tutabridge" \
      org.opencontainers.image.version="${TUTABRIDGE_VERSION}" \
      org.opencontainers.image.revision="${TUTABRIDGE_REVISION}" \
      org.opencontainers.image.created="${TUTABRIDGE_CREATED}" \
      org.opencontainers.image.licenses="GPL-3.0-or-later" \
      org.opencontainers.image.title="TutaBridge" \
      org.opencontainers.image.description="IMAP/SMTP bridge for Tuta encrypted email"
# libdbus is still linked by the Linux keyring backend, although this image
# explicitly selects pass. No desktop session or Secret Service is needed.
RUN apt-get update && apt-get install --no-install-recommends -y \
    ca-certificates pass gnupg git libdbus-1-3 util-linux tini \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --create-home --shell /usr/sbin/nologin tutabridge \
    && install -d -m 0700 -o 10001 -g 10001 /data
COPY --from=builder /build/tutabridge /usr/local/bin/tutabridge
COPY --chmod=755 docker/entrypoint.sh docker/bootstrap.sh /usr/local/lib/tutabridge/
# Shortcuts for docker exec: `init` and `password` run the same CLI commands.
COPY --chmod=755 docker/bin/init docker/bin/password /usr/local/bin/
COPY docker/config.toml docker/gpg-key-parameters /usr/local/lib/tutabridge/
COPY LICENSE /usr/share/doc/tutabridge/LICENSE
ENV HOME=/data \
    TUTABRIDGE_CONTAINER=1 \
    TUTABRIDGE_DATA_DIR=/data \
    TUTABRIDGE_RUNTIME_DIR=/tmp/tutabridge \
    PASSWORD_STORE_GPG_OPTS="--batch --pinentry-mode error" \
    XDG_CONFIG_HOME=/data/config \
    XDG_CACHE_HOME=/data/cache \
    GNUPGHOME=/data/gnupg \
    PASSWORD_STORE_DIR=/data/pass
USER 10001:10001
VOLUME ["/data"]
EXPOSE 1143 1025
HEALTHCHECK --interval=30s --timeout=5s --start-period=30s --retries=3 CMD ["tutabridge", "health"]
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/lib/tutabridge/entrypoint.sh"]
CMD ["run"]
