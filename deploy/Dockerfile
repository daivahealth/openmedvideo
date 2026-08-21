# Multi-stage build for both services. Build context is the repo root:
#   docker build -f deploy/Dockerfile --target api ..
#   docker build -f deploy/Dockerfile --target worker ..

FROM rust:1-slim-bookworm AS builder
RUN apt-get update && apt-get install -y --no-install-recommends pkg-config libssl-dev && rm -rf /var/lib/apt/lists/*
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --workspace

FROM debian:bookworm-slim AS api
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/omv-api /usr/local/bin/
EXPOSE 8080
CMD ["omv-api"]

FROM debian:bookworm-slim AS worker
# ffmpeg does the heavy lifting (design §4.3); libx264 comes with it.
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates ffmpeg && rm -rf /var/lib/apt/lists/*
COPY --from=builder /src/target/release/omv-worker /usr/local/bin/
CMD ["omv-worker"]
