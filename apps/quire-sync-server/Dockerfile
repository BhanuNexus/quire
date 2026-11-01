# syntax=docker/dockerfile:1
# Build from the repository root:
#   docker build -f apps/quire-sync-server/Dockerfile -t quire-sync-server .
#
# Run (keep the database on a volume, put HTTPS in front):
#   docker run -d --name quire-sync -p 8787:8787 \
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
USER quire
WORKDIR /data
ENV QUIRE_SYNC_ADDR=0.0.0.0:8787 QUIRE_SYNC_DB=/data/quire-sync.sqlite
EXPOSE 8787
VOLUME ["/data"]
HEALTHCHECK --interval=30s --timeout=3s CMD ["quire-sync-server", "--version"]
ENTRYPOINT ["quire-sync-server"]
