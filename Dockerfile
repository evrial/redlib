# Use the official cargo-zigbuild image which natively cross-compiles all targets from any host
FROM --platform=$BUILDPLATFORM ghcr.io/rust-cross/cargo-zigbuild:latest AS builder

RUN rustup default stable

RUN apt-get update && apt-get install -y --no-install-recommends \
    git \
    make \
    cmake \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /redlib

# Injected automatically by Docker Buildx for each platform
ARG TARGETPLATFORM

# Helper script block to map TARGETPLATFORM to Rust target cleanly
RUN case "${TARGETPLATFORM}" in \
        "linux/amd64")   echo "RUST_TARGET=x86_64-unknown-linux-musl" >> /etc/environment ;; \
        "linux/arm64")   echo "RUST_TARGET=aarch64-unknown-linux-musl" >> /etc/environment ;; \
        "linux/arm/v7")  echo "RUST_TARGET=armv7-unknown-linux-musleabihf" >> /etc/environment ;; \
        "linux/riscv64") echo "RUST_TARGET=riscv64gc-unknown-linux-musl" >> /etc/environment ;; \
        *) echo "Unsupported platform: ${TARGETPLATFORM}" && exit 1 ;; \
    esac

# 1. Copy full source code directly (cargo caching makes dependency compilation fast anyway)
COPY . .

# 2. Add rust target and build release binary
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=reg-${TARGETPLATFORM} \
    --mount=type=cache,target=/redlib/target,id=target-${TARGETPLATFORM} \
    . /etc/environment && \
    echo "Building for target: ${RUST_TARGET}" && \
    rustup target add "${RUST_TARGET}" && \
    cargo zigbuild --release --target "${RUST_TARGET}" --bin redlib && \
    cp target/"${RUST_TARGET}"/release/redlib /usr/local/bin/redlib

########################
## Release Image (Distroless)
########################
# Using the nonroot variant which includes CA certificates and a pre-configured non-root user (UID 65532)
FROM gcr.io/distroless/static-debian12:nonroot

COPY --from=builder /usr/local/bin/redlib /usr/local/bin/redlib

EXPOSE 8080

LABEL org.opencontainers.image.title="Redlib" \
      org.opencontainers.image.description="Private front-end for Reddit written in Rust" \
      org.opencontainers.image.url="https://github.com/redlib-org/redlib" \
      org.opencontainers.image.source="https://github.com/redlib-org/redlib" \
      org.opencontainers.image.licenses="AGPL-3.0"

USER nonroot:nonroot

ENTRYPOINT ["/usr/local/bin/redlib"]
