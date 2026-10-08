# Default image that agents run in. Add the toolchains your projects need.
#
#   docker build -t agentcore-sandbox:latest sandbox-image                       # opencode only
#   docker build --build-arg AGENTS="codex goose" -t agentcore-sandbox:latest sandbox-image
#   docker build --build-arg AGENTS=all -t agentcore-sandbox:full sandbox-image
#
# AGENTS names entries of the agent catalogue (templates/agents/); see
# docs/AGENT_CATALOG.md for what each one is and its license.
FROM node:22-bookworm-slim

ARG AGENTS=opencode

RUN apt-get update \
 && apt-get install -y --no-install-recommends \
      ca-certificates coreutils curl git build-essential pkg-config python3 python3-pip \
      ripgrep bzip2 \
 && rm -rf /var/lib/apt/lists/*

COPY install-agents.sh /usr/local/bin/agentcore-install-agents
RUN AGENTS="${AGENTS}" sh /usr/local/bin/agentcore-install-agents

# The node image's own user has uid 1000; agentcore runs agents as `agent`.
RUN userdel --remove node \
 && useradd --uid 1000 --create-home --home-dir /home/agent agent
USER 1000:1000
WORKDIR /workspace
