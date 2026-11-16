# syntax=docker/dockerfile:1
# Build from the repository root:
#   docker build -f apps/quire-sync-server/Dockerfile -t quire-sync-server .
#
# Run (keep the database on a volume, put HTTPS in front). The server speaks
# plain HTTP, so publish it on this machine only (127.0.0.1) and let a reverse
# proxy such as Caddy terminate HTTPS; never publish it as -p 8787:8787, which
# listens on every interface. See docs/self-hosting.md.
#   docker run -d --name quire-sync --restart unless-stopped -p 127.0.0.1:8787:8787 \
#     -e QUIRE_SYNC_TOKEN="$(openssl rand -hex 32)" \
#     -v quire-sync:/data quire-sync-server

FROM rust:1.97.0-slim-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release --locked -p quire-sync-server

FROM debian:bookworm-slim
RUN useradd --system --uid 10001 --home-dir /data quire && mkdir -p /data && chown quire /data
COPY --from=build /src/target/release/quire-sync-server /usr/local/bin/quire-sync-server
COPY LICENSE /usr/share/doc/quire-sync-server/LICENSE
COPY apps/quire-sync-server/healthcheck.sh /usr/local/bin/healthcheck
USER quire
WORKDIR /data
ENV QUIRE_SYNC_ADDR=0.0.0.0:8787 QUIRE_SYNC_DB=/data/quire-sync.sqlite
EXPOSE 8787
VOLUME ["/data"]
# Asks the running server for GET /health (the program has no curl here).
HEALTHCHECK --interval=30s --timeout=3s --start-period=10s CMD ["healthcheck"]
ENTRYPOINT ["quire-sync-server"]
