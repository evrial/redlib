# supported versions here: https://hub.docker.com/_/rust
ARG ALPINE_VERSION=3.23

# Use the official Rust image as a stable base for the builder
FROM --platform=$BUILDPLATFORM rust:alpine${ALPINE_VERSION} AS builder

# Install zig and cargo-zigbuild
RUN apk add --no-cache zig cargo-zigbuild musl-dev git make perl file
RUN rustup component add rust-src

WORKDIR /redlib
COPY . .

# Injected by Docker Buildx
ARG TARGETARCH
ARG TARGETVARIANT

# Map Docker architectures to Rust/Zig musl triples
RUN case "${TARGETARCH}${TARGETVARIANT}" in \
        "amd64")    export T="x86_64-unknown-linux-musl" ;; \
        "arm64")    export T="aarch64-unknown-linux-musl" ;; \
        "armv7")    export T="armv7-unknown-linux-musleabihf" ;; \
        "riscv64")  export T="riscv64gc-unknown-linux-musl" ;; \
        "s390x")    export T="s390x-unknown-linux-musl" ;; \
        "ppc64le")  export T="powerpc64le-unknown-linux-musl" ;; \
        "386")      export T="i686-unknown-linux-musl" ;; \
        *) echo "Unsupported: ${TARGETARCH}${TARGETVARIANT}"; exit 1 ;; \
    esac && \
    rustup target add "$T" || true && \
    # We use -Z build-std to compile the standard library for the target on the fly
    # This bypasses the "no prebuilt artifacts" error
    cargo zigbuild --release --target "$T" -Z build-std --bin redlib && \
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
