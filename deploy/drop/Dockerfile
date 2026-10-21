# rim-drop: the drop for "send to someone" (PRD HB-3). It keeps only ciphertext;
# the key travels in the link, after the '#', and never reaches this server.
#   docker build -f deploy/drop/Dockerfile -t rim-drop .
# See deploy/drop/compose.yaml for running it behind a Cloudflare tunnel.
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p rim-drop --features server

FROM debian:bookworm-slim
RUN useradd --system --home /data --shell /usr/sbin/nologin rimdrop \
 && mkdir -p /data && chown rimdrop:rimdrop /data
COPY --from=build /src/target/release/rim-drop /usr/local/bin/rim-drop
USER rimdrop
VOLUME /data
EXPOSE 8080
ENTRYPOINT ["rim-drop", "--dir", "/data", "--listen", "0.0.0.0:8080"]
