#!/usr/bin/env bash
# Isolated dev stack, safe to run next to a production agentcore on the same VM.
#
#   scripts/dev.sh                      UI on 0.0.0.0:<random port>
#   DEV_BIND=127.0.0.1 scripts/dev.sh   loopback only (use an SSH tunnel)
#   DEV_PORT_RANGE=30000-40000 ...      pick the random port from another range
#
# Nothing is shared with production: own Postgres container + volume (no host
# port), own data dir in .dev/, own operator token, backend on loopback only.
set -euo pipefail
cd "$(dirname "$0")/.."
[ -f "$HOME/.cargo/env" ] && . "$HOME/.cargo/env"

BIND=${DEV_BIND:-0.0.0.0}
RANGE=${DEV_PORT_RANGE:-20000-60000}
DEV=.dev
PG=agentcore-dev-postgres
mkdir -p "$DEV/data"

free_port() {
  local p
  for _ in $(seq 50); do
    p=$(shuf -i "$RANGE" -n 1)
    ss -tln | grep -q ":$p " || { echo "$p"; return; }
  done
  echo "no free port in $RANGE" >&2; exit 1
}

[ -f "$DEV/token" ] || { openssl rand -hex 24 > "$DEV/token"; chmod 600 "$DEV/token"; }
[ -f "$DEV/pgpass" ] || { openssl rand -hex 16 > "$DEV/pgpass"; chmod 600 "$DEV/pgpass"; }
TOKEN=$(cat "$DEV/token"); PGPASS=$(cat "$DEV/pgpass")

# Dev Postgres: reached by container IP, never published on the host.
if ! docker inspect "$PG" >/dev/null 2>&1; then
  docker run -d --name "$PG" -v agentcore-dev-pgdata:/var/lib/postgresql/data \
    -e POSTGRES_USER=agentcore -e POSTGRES_PASSWORD="$PGPASS" -e POSTGRES_DB=agentcore \
    postgres:16 >/dev/null
else
  docker start "$PG" >/dev/null
fi
until docker exec "$PG" pg_isready -U agentcore -q 2>/dev/null; do sleep 1; done
PGIP=$(docker inspect -f '{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}' "$PG")

API_PORT=$(free_port)
UI_PORT=$(free_port)
HASH=$(printf %s "$TOKEN" | sha256sum | cut -d' ' -f1)

sed -e "s|^bind = .*|bind = \"127.0.0.1:$API_PORT\"|" \
    -e "s|^url = .*|url = \"postgres://agentcore:$PGPASS@$PGIP:5432/agentcore\"|" \
    -e "s|^data_dir = .*|data_dir = \"$PWD/$DEV/data\"|" \
    -e "s|^ui_dir = .*|ui_dir = \"$PWD/web/dist\"|" \
    -e "s|^dir = \"policies\"|dir = \"$PWD/policies\"|" \
    -e "s|^dir = \"roles\"|dir = \"$PWD/roles\"|" \
    agentcore.dev.toml > "$DEV/agentcore.toml"
cat >> "$DEV/agentcore.toml" <<TOML

[templates]
dir = "$PWD/templates"

[[server.operators]]
name = "dev"
token_sha256 = "$HASH"
role = "admin"
TOML

pids=()
cleanup() { kill "${pids[@]}" 2>/dev/null || true; docker stop "$PG" >/dev/null 2>&1 || true; }
trap cleanup EXIT INT TERM

cargo run -q -p agentcore-cli -- serve --config "$DEV/agentcore.toml" &
pids+=($!)
until curl -s -o /dev/null "http://127.0.0.1:$API_PORT/"; do
  kill -0 "${pids[0]}" 2>/dev/null || { echo "backend exited" >&2; exit 1; }
  sleep 1
done

[ -d web/node_modules ] || (cd web && npm ci)
(cd web && AGENTCORE_DEV_API="http://127.0.0.1:$API_PORT" \
  exec npx vite --host "$BIND" --port "$UI_PORT" --strictPort) &
pids+=($!)

cat <<MSG

  Web UI:  http://<this-vm>:$UI_PORT   (bound to $BIND)
  Token:   $TOKEN   (also in $DEV/token)
  API:     127.0.0.1:$API_PORT (loopback only)
  Tunnel:  ssh -L $UI_PORT:127.0.0.1:$UI_PORT <user>@<this-vm>   (with DEV_BIND=127.0.0.1)

MSG
wait -n
