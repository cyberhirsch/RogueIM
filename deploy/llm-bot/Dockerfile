# rim-llm-bot: an ad-financed AI contact for RogueIM users (e.g. on a Raspberry Pi).
#   docker build -f deploy/llm-bot/Dockerfile -t rim-llm-bot .
#   docker run -d --name rim-llm-bot --restart unless-stopped \
#     -v rim-llm-bot:/data -e RIM_PASS='profile passphrase' -e GEMINI_API_KEY='…' rim-llm-bot
# No port needs to be opened: the bot connects outwards (hole punching, public
# helpers, Nostr), like any RogueIM. Forwarding 4002 tcp+udp only makes direct
# connections a little more likely.
# Optional: put a bot.json (name, personality, ad text, limits) into the /data volume.
FROM rust:1-bookworm AS build
WORKDIR /src
RUN apt-get update && apt-get install -y --no-install-recommends cmake && rm -rf /var/lib/apt/lists/*
COPY . .
RUN cargo build --release -p rim-llm-bot

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/* \
 && useradd --system --home /data --shell /usr/sbin/nologin rogueim
COPY --from=build /src/target/release/rim-llm-bot /usr/local/bin/rim-llm-bot
USER rogueim
VOLUME /data
EXPOSE 4002/tcp 4002/udp
ENTRYPOINT ["sh", "-c", "if [ -f /data/bot.json ]; then exec rim-llm-bot --dir /data/profile --port 4002 --config /data/bot.json \"$@\"; else exec rim-llm-bot --dir /data/profile --port 4002 \"$@\"; fi", "--"]
