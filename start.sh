#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

DIST_ENTRY="$SCRIPT_DIR/dist/server.js"
BACKEND_ENTRY="$SCRIPT_DIR/dist/local-backend/server.js"
NODE_BIN="$(command -v node || true)"
if [[ -z "$NODE_BIN" ]]; then
  echo "[index-mcp] Error: node is not on PATH. Install Node.js 18+ and try again." >&2
  exit 1
fi

if [[ ! -f "$DIST_ENTRY" || ! -f "$BACKEND_ENTRY" ]]; then
  echo "[index-mcp] Build output missing; running npm run build..." >&2
  if ! command -v npm >/dev/null 2>&1; then
    echo "[index-mcp] Error: npm is not on PATH to build the project." >&2
    exit 1
  fi
  (cd "$SCRIPT_DIR" && npm run build) >&2
fi

NATIVE_DIR="$SCRIPT_DIR/crates/index_mcp_native"
if ! command -v npm >/dev/null 2>&1; then
  echo "[index-mcp] Error: npm is not on PATH to build the native addon." >&2
  exit 1
fi

if [[ ! -d "$NATIVE_DIR/node_modules" ]]; then
  echo "[index-mcp] Installing native addon dependencies..." >&2
  (cd "$NATIVE_DIR" && npm install) >&2
fi

echo "[index-mcp] Building native addon in release mode..." >&2
(cd "$NATIVE_DIR" && npm run build) >&2

BACKEND_HOST="${LOCAL_BACKEND_HOST:-127.0.0.1}"
BACKEND_PORT="${LOCAL_BACKEND_PORT:-8765}"

cleanup() {
  local exit_status=$?
  if [[ -n "${BACKEND_PID:-}" ]]; then
    if kill -0 "$BACKEND_PID" >/dev/null 2>&1; then
      kill "$BACKEND_PID" >/dev/null 2>&1 || true
      wait "$BACKEND_PID" >/dev/null 2>&1 || true
    fi
  fi
  return $exit_status
}

trap cleanup EXIT

echo "[index-mcp] Launching local backend on ${BACKEND_HOST}:${BACKEND_PORT}" >&2
"$NODE_BIN" "$BACKEND_ENTRY" &
BACKEND_PID=$!

# Wait for backend readiness
if command -v curl >/dev/null 2>&1; then
  backend_ready=0
  for attempt in $(seq 1 40); do
    if curl --silent --fail "http://${BACKEND_HOST}:${BACKEND_PORT}/healthz" >/dev/null 2>&1; then
      backend_ready=1
      break
    fi
    sleep 0.25
  done
  if [[ "$backend_ready" -ne 1 ]]; then
    echo "[index-mcp] Backend failed to become ready at http://${BACKEND_HOST}:${BACKEND_PORT}" >&2
    exit 1
  fi
else
  # Fallback: allow backend a moment to boot if curl is unavailable
  sleep 1
fi

echo "[index-mcp] Launching MCP server from $DIST_ENTRY" >&2
"$NODE_BIN" "$DIST_ENTRY"
