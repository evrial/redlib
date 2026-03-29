# supported versions here: https://hub.docker.com/_/rust
ARG ALPINE_VERSION=3.22

FROM rust:alpine${ALPINE_VERSION} AS builder

RUN apk add --no-cache make musl-dev

WORKDIR /redlib
COPY . .

RUN cargo build --release --locked --bin redlib

FROM alpine:${ALPINE_VERSION} AS release

RUN apk add --no-cache ca-certificates

COPY --from=builder /redlib/target/release/redlib /usr/local/bin/redlib

RUN adduser --home /nonexistent --no-create-home --disabled-password redlib
USER redlib

EXPOSE 8080

HEALTHCHECK --interval=1m --timeout=3s CMD wget --spider -q http://localhost:8080/settings || exit 1

CMD ["redlib"]
