# syntax=docker/dockerfile:1

FROM ghcr.io/voidzero-dev/vite-plus:1.4.0 AS website
WORKDIR /app
COPY --chown=vp:vp . .
RUN vp install --frozen-lockfile && vp run website#build

FROM rust:1-bookworm AS server
RUN apt-get update && apt-get install -y --no-install-recommends cmake \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY apps/server/Cargo.toml apps/server/Cargo.lock ./
COPY apps/server/src ./src
RUN cargo build --locked --release

FROM debian:bookworm-slim AS runtime
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=server /app/target/release/hokan-server /usr/local/bin/hokan-server
COPY --from=website /app/apps/website/dist ./website
ENV HOKAN_ADDRESS=0.0.0.0:3000 \
    HOKAN_ASSETS_DIR=/app/website
USER nobody
EXPOSE 3000
CMD ["hokan-server"]
