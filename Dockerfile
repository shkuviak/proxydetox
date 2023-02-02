# Build step
FROM rust:latest AS build

# Copy sources
COPY ./ /build

# Install clang
RUN apt update
RUN apt install clang -y

# Build app
RUN cd /build && cargo build --features=negotiate -r

# Release packaging
FROM debian:stable-slim AS prod

COPY --from=build /build/target/release/proxydetox /usr/local/bin/

RUN apt update
RUN apt install krb5-user -y

ENTRYPOINT ["proxydetox","--interface", "0.0.0.0"]