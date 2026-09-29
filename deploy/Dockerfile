# RogueIM always-on node: relay, DHT server and mailbox helper for your friends.
#   docker build -f deploy/Dockerfile -t rogueim-node .
#   docker run -d --name rim-node -p 4001:4001/tcp -p 4001:4001/udp \
#     -v rim-node:/data -e RIM_PASS='change me' rogueim-node
FROM rust:1-bookworm AS build
WORKDIR /src
COPY . .
RUN cargo build --release -p rim-cli

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
 && useradd --system --home /data --shell /usr/sbin/nologin rogueim
COPY --from=build /src/target/release/rim-cli /usr/local/bin/rim-cli
USER rogueim
VOLUME /data
EXPOSE 4001/tcp 4001/udp
ENV RIM_PASS=""
ENTRYPOINT ["sh", "-c", "exec rim-cli --node --dir /data --port 4001 --pass \"${RIM_PASS:?set RIM_PASS}\""]
