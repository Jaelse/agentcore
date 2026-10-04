# agentcore server image: Rust binary + built React UI + docker CLI.

FROM node:22-bookworm-slim AS web
WORKDIR /src/web
COPY web/package.json web/package-lock.json ./
RUN npm ci
COPY web/ ./
RUN npm run build

FROM rust:1-bookworm AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
RUN cargo build --release --locked -p agentcore-cli

FROM debian:bookworm-slim
RUN apt-get update \
 && apt-get install -y --no-install-recommends ca-certificates docker.io curl \
 && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/release/agentcore /usr/local/bin/agentcore
COPY --from=web /src/web/dist /opt/agentcore/web/dist
COPY policies /opt/agentcore/policies
WORKDIR /opt/agentcore
EXPOSE 8080
HEALTHCHECK CMD curl -fsS http://127.0.0.1:8080/api/v1/health || exit 1
ENTRYPOINT ["agentcore", "--log-format", "json"]
CMD ["serve", "--config", "/etc/agentcore/agentcore.toml"]
