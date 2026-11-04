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

FROM debian:bookworm-slim AS runtime
# libdbus is still linked by the Linux keyring backend, although this image
# explicitly selects pass. No desktop session or Secret Service is needed.
RUN apt-get update && apt-get install --no-install-recommends -y \
    ca-certificates pass gnupg libdbus-1-3 util-linux tini \
    && rm -rf /var/lib/apt/lists/* \
    && useradd --uid 10001 --create-home --shell /usr/sbin/nologin tutabridge \
    && install -d -m 0700 -o 10001 -g 10001 /data
COPY --from=builder /build/tutabridge /usr/local/bin/tutabridge
COPY --chmod=755 docker/entrypoint.sh /usr/local/lib/tutabridge/entrypoint.sh
COPY docker/config.toml docker/gpg-key-parameters /usr/local/lib/tutabridge/
COPY LICENSE /usr/share/doc/tutabridge/LICENSE
ENV XDG_CONFIG_HOME=/data/config \
    XDG_CACHE_HOME=/data/cache \
    GNUPGHOME=/data/gnupg \
    PASSWORD_STORE_DIR=/data/pass
USER 10001:10001
VOLUME ["/data"]
EXPOSE 1143 1025
STOPSIGNAL SIGTERM
ENTRYPOINT ["/usr/bin/tini", "--", "/usr/local/lib/tutabridge/entrypoint.sh"]
CMD ["run"]
