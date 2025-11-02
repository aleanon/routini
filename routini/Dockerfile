# Start with image that has the Rust toolchain installed
FROM rust:1.85-alpine AS builder
USER root
RUN apk add --no-cache musl-dev cmake make g++
WORKDIR /app

# Copy everything (needed for path dependencies)
COPY . .

# Build the routini binary
WORKDIR /app/routini
RUN cargo build --release --bin routini

# We do not need the Rust toolchain to run the binary!
# Start with a minimal image and copy over the binary and assets folder.
FROM debian:buster-slim AS runtime
WORKDIR /app
COPY --from=builder /app/routini/target/release/routini /usr/local/bin
ENTRYPOINT ["/usr/local/bin/routini"]
