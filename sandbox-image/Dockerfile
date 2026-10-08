# Default image that agents run in. Add the toolchains your projects need.
FROM debian:bookworm-slim

ARG OPENCODE_VERSION=latest

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates coreutils curl git build-essential pkg-config python3 nodejs npm ripgrep \
 && rm -rf /var/lib/apt/lists/*

# opencode (optional: remove if you only run other agents).
RUN npm install -g "opencode-ai@${OPENCODE_VERSION}" && npm cache clean --force

RUN useradd --uid 1000 --create-home --home-dir /home/agent agent
USER 1000:1000
WORKDIR /workspace
