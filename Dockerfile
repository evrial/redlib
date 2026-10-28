# Use the official cargo-zigbuild image which natively cross-compiles all targets from any host
FROM --platform=$BUILDPLATFORM ghcr.io/rust-cross/cargo-zigbuild:latest AS builder
RUN rustup default stable

RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    make \
    cmake \
    perl \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /redlib

# Injected automatically by Docker Buildx for each platform
ARG TARGETPLATFORM

# 1. Copy full source code directly
COPY . .

# 2. Map target platform, add rust target, and build release binary using BuildKit cache mounts
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=reg-${TARGETPLATFORM} \
    --mount=type=cache,target=/redlib/target,id=target-${TARGETPLATFORM} \
    set -e; \
    case "${TARGETPLATFORM}" in \
        "linux/amd64")    RUST_TARGET="x86_64-unknown-linux-musl" ;; \
        "linux/arm64")    RUST_TARGET="aarch64-unknown-linux-musl" ;; \
        "linux/arm/v7")   RUST_TARGET="armv7-unknown-linux-musleabihf" ;; \
        "linux/riscv64")  RUST_TARGET="riscv64gc-unknown-linux-musl" ;; \
        *) echo "Unsupported platform: ${TARGETPLATFORM}" && exit 1 ;; \
    esac; \
    echo "Building for target: ${RUST_TARGET}" && \
    rustup target add "${RUST_TARGET}" && \
    cargo zigbuild --release --target "${RUST_TARGET}" --bin redlib && \
    cp target/"${RUST_TARGET}"/release/redlib /usr/local/bin/redlib

########################
## Release Image
########################
FROM alpine AS release
RUN apk add --no-cache ca-certificates
COPY --from=builder /usr/local/bin/redlib /usr/local/bin/redlib

# Security: Non-root user
RUN adduser --home /nonexistent --no-create-home --disabled-password redlib
USER redlib

EXPOSE 8080
HEALTHCHECK --interval=1m --timeout=3s CMD wget --spider -q http://localhost:8080/settings || exit 1

LABEL org.opencontainers.image.title="Redlib" \
      org.opencontainers.image.description="Private front-end for Reddit written in Rust" \
      org.opencontainers.image.url="https://github.com/redlib-org/redlib" \
      org.opencontainers.image.source="https://github.com/redlib-org/redlib" \
      org.opencontainers.image.licenses="AGPL-3.0"

CMD ["/usr/local/bin/redlib"]
