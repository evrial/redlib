# supported versions here: https://hub.docker.com/_/rust
ARG ALPINE_VERSION=3.23

# Use the official Rust image as a stable base for the builder
FROM --platform=$BUILDPLATFORM rust:alpine${ALPINE_VERSION} AS builder

# Install zig and cargo-zigbuild
RUN apk add --no-cache zig cargo-zigbuild musl-dev git make perl file

# 2. Switch to Nightly and add the source component
RUN rustup toolchain install nightly && \
    rustup default nightly && \
    rustup component add rust-src --toolchain nightly

WORKDIR /redlib
COPY . .

# Injected by Docker Buildx
ARG TARGETPLATFORM

# Map Docker architectures to Rust/Zig musl triples
RUN --mount=type=cache,target=/usr/local/cargo/registry,id=reg-${TARGETPLATFORM} \
    --mount=type=cache,target=/redlib/target,id=target-${TARGETPLATFORM} \
    case "${TARGETPLATFORM}" in \
        "linux/amd64")    export T="x86_64-unknown-linux-musl" ;; \
        "linux/arm64")    export T="aarch64-unknown-linux-musl" ;; \
        "linux/arm/v7")   export T="armv7-unknown-linux-musleabihf" ;; \
        "linux/arm/v6")   export T="arm-unknown-linux-musleabi" ;; \
        "linux/riscv64")  export T="riscv64gc-unknown-linux-musl" ;; \
        "linux/s390x")    export T="s390x-unknown-linux-musl" ;; \
        "linux/ppc64le")  export T="powerpc64le-unknown-linux-musl" ;; \
        "linux/386")      export T="i686-unknown-linux-musl" ;; \
        *) echo "Unsupported platform: ${TARGETPLATFORM}"; exit 1 ;; \
    esac && \
    # We use -C link-self-contained=no to stop Rust from looking for crt1.o
    # Zig will provide these files from its own internal musl sysroot.
    RUSTFLAGS="-C link-self-contained=no -C target-feature=-crt-static" \
    cargo zigbuild --release \
        --target "$T" \
        -Z build-std=std,core,alloc,panic_unwind \
        -Z build-std-features=compiler-builtins-mem \
        --bin redlib && \
    cp target/"$T"/release/redlib /usr/local/bin/redlib

# Final verification of the binary architecture
RUN file /usr/local/bin/redlib

########################
## Release Image
########################
FROM alpine:${ALPINE_VERSION} AS release
RUN apk add --no-cache ca-certificates
COPY --from=builder /usr/local/bin/redlib /usr/local/bin/redlib

# Security: Non-root user
RUN adduser --home /nonexistent --no-create-home --disabled-password redlib
USER redlib

EXPOSE 8080
HEALTHCHECK --interval=1m --timeout=3s CMD wget --spider -q http://localhost:8080/settings || exit 1

CMD ["/usr/local/bin/redlib"]
