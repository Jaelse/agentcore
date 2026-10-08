#!/bin/sh
# Install agents from agentcore's agent catalogue (templates/agents/) into
# the sandbox image. Versions are the ones agentcore's integration was
# verified with; change them deliberately.
#
#   AGENTS="opencode codex goose"   install these
#   AGENTS=all                      install every catalogue agent
#
# Everything comes from the projects' official channels (npm, PyPI, GitHub
# releases), unmodified, with their license files.
set -eu

AGENTS="${AGENTS:-opencode}"
[ "$AGENTS" = all ] && AGENTS="opencode codex qwen-code goose fast-agent aider mini-swe-agent"

OPENCODE_VERSION="${OPENCODE_VERSION:-1.18.35}"
CODEX_VERSION="${CODEX_VERSION:-0.161.0}"
QWEN_CODE_VERSION="${QWEN_CODE_VERSION:-0.25.0}"
GOOSE_VERSION="${GOOSE_VERSION:-1.53.0}"
FAST_AGENT_VERSION="${FAST_AGENT_VERSION:-0.10.43}"
AIDER_VERSION="${AIDER_VERSION:-0.86.2}"
MINI_SWE_AGENT_VERSION="${MINI_SWE_AGENT_VERSION:-2.4.6}"

# Python agents live in their own virtual environments (via uv), with their
# own Python where they need a newer one than the image has.
export UV_TOOL_DIR=/opt/agents/uv UV_TOOL_BIN_DIR=/usr/local/bin UV_PYTHON_INSTALL_DIR=/opt/agents/python
LICENSES=/usr/share/doc/agentcore-agents
mkdir -p "$LICENSES"

uv_tool() { # package==version python
  command -v uv >/dev/null || pip3 install --no-cache-dir --break-system-packages uv
  uv tool install --python "$2" "$1"
}

for agent in $AGENTS; do
  echo "==> $agent"
  case "$agent" in
    opencode)       npm install -g "opencode-ai@${OPENCODE_VERSION}" ;;
    codex)          npm install -g "@openai/codex@${CODEX_VERSION}" ;;
    qwen-code)      npm install -g "@qwen-code/qwen-code@${QWEN_CODE_VERSION}" ;;
    fast-agent)     uv_tool "fast-agent-mcp==${FAST_AGENT_VERSION}" 3.12 ;;
    aider)          uv_tool "aider-chat==${AIDER_VERSION}" 3.12 ;;
    mini-swe-agent) uv_tool "mini-swe-agent==${MINI_SWE_AGENT_VERSION}" 3.12 ;;
    goose)
      case "$(uname -m)" in
        x86_64) arch=x86_64 ;;
        aarch64 | arm64) arch=aarch64 ;;
        *) echo "goose: unsupported architecture $(uname -m)" >&2; exit 1 ;;
      esac
      tmp=$(mktemp -d)
      curl -fsSL "https://github.com/block/goose/releases/download/v${GOOSE_VERSION}/goose-${arch}-unknown-linux-gnu.tar.bz2" \
        | tar -xj -C "$tmp"
      install -m 0755 "$(find "$tmp" -type f -name goose | head -n 1)" /usr/local/bin/goose
      # The release binary carries no license file: ship the project's.
      curl -fsSL "https://raw.githubusercontent.com/block/goose/v${GOOSE_VERSION}/LICENSE" -o "$LICENSES/goose-LICENSE" \
        || echo "goose is Apache-2.0: https://github.com/block/goose/blob/main/LICENSE" > "$LICENSES/goose-LICENSE"
      rm -rf "$tmp"
      ;;
    *) echo "unknown agent '$agent' (see templates/agents/)" >&2; exit 1 ;;
  esac
  echo "$agent" >> "$LICENSES/installed"
done
npm cache clean --force >/dev/null 2>&1 || true
rm -rf /root/.cache
