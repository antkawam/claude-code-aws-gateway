# syntax=docker/dockerfile:1
# Build stage — BuildKit cache mounts persist cargo registry + target across builds
FROM rust:1-alpine AS builder
RUN apk add --no-cache musl-dev
WORKDIR /app

# Copy manifests first for better layer caching
COPY Cargo.toml Cargo.lock ./
COPY cli/Cargo.toml cli/Cargo.toml

# Pre-build dependencies with dummy sources (cached unless Cargo.toml/Cargo.lock change)
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    mkdir -p src cli/src && \
    echo 'fn main() {}' > src/main.rs && \
    echo 'fn main() {}' > cli/src/main.rs && \
    cargo build --release --bin ccag-server 2>&1 | tail -1 && \
    rm -rf src cli/src

# Copy source and build (incremental — only recompiles project code)
COPY src/ src/
COPY static/ static/
COPY migrations/ migrations/
COPY .sqlx/ .sqlx/
COPY build.rs build.rs
ENV SQLX_OFFLINE=true
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/usr/local/cargo/git \
    --mount=type=cache,target=/app/target \
    cargo build --release --bin ccag-server && \
    cp /app/target/release/ccag-server /usr/local/bin/ccag-server

# Runtime stage — minimal Alpine
FROM alpine:3.21
RUN apk add --no-cache ca-certificates curl postgresql16-client && \
    addgroup -S proxy && adduser -S proxy -G proxy
COPY --from=builder /usr/local/bin/ccag-server /usr/local/bin/
USER proxy
EXPOSE 8080
ENV PROXY_HOST=0.0.0.0
ENV PROXY_PORT=8080
CMD ["ccag-server"]
