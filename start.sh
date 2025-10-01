#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

RUNTIME="${INDEX_MCP_RUNTIME:-rust}"

if [[ "$RUNTIME" == "rust" ]]; then
  CARGO_BIN="$(command -v cargo || true)"
  if [[ -z "$CARGO_BIN" ]]; then
    echo "[index-mcp] Error: cargo is not on PATH. Install the Rust toolchain and try again." >&2
    exit 1
  fi

  PROFILE="${INDEX_MCP_CARGO_PROFILE:-release}"
  if [[ -n "${INDEX_MCP_ARGS:-}" ]]; then
    # shellcheck disable=SC2206
    EXTRA_ARGS=( ${INDEX_MCP_ARGS} )
  else
    EXTRA_ARGS=()
  fi

  CMD=("$CARGO_BIN" run --manifest-path "$SCRIPT_DIR/Cargo.toml" -p index-mcp-server)
  case "$PROFILE" in
    release)
      CMD+=(--release)
      ;;
    debug)
      ;;
    *)
      CMD+=(--profile "$PROFILE")
      ;;
  esac

  if [[ ${#EXTRA_ARGS[@]} -gt 0 ]]; then
    CMD+=(-- "${EXTRA_ARGS[@]}")
  fi

  echo "[index-mcp] Launching Rust MCP server (profile: $PROFILE)" >&2
  cd "$SCRIPT_DIR"
  exec "${CMD[@]}"
fi

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
BACKEND_BASE_URL="http://${BACKEND_HOST}:${BACKEND_PORT}"
BACKEND_HEALTH_URL="${BACKEND_BASE_URL}/healthz"

HAS_CURL=0
if command -v curl >/dev/null 2>&1; then
  HAS_CURL=1
fi

check_backend_health() {
  if [[ "$HAS_CURL" -ne 1 ]]; then
    return 1
  fi
  local response
  if ! response="$(curl --silent --fail "$BACKEND_HEALTH_URL" 2>/dev/null)"; then
    return 1
  fi
  case "$response" in
    *'"status":"ok"'*) return 0 ;;
  esac
  return 1
}

reuse_existing_backend=0
if [[ "$HAS_CURL" -eq 1 ]]; then
  if check_backend_health; then
    reuse_existing_backend=1
    echo "[index-mcp] Reusing existing local backend at ${BACKEND_BASE_URL}" >&2
  fi
fi

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

if [[ "$reuse_existing_backend" -eq 0 ]]; then
  echo "[index-mcp] Launching local backend on ${BACKEND_HOST}:${BACKEND_PORT}" >&2
  "$NODE_BIN" "$BACKEND_ENTRY" &
  BACKEND_PID=$!
else
  BACKEND_PID=""
fi

# Wait for backend readiness
if [[ "$HAS_CURL" -eq 1 ]]; then
  backend_ready=0
  for attempt in $(seq 1 40); do
    if check_backend_health; then
      backend_ready=1
      break
    fi
    sleep 0.25
  done
  if [[ "$backend_ready" -ne 1 ]]; then
    if [[ "$reuse_existing_backend" -eq 1 ]]; then
      echo "[index-mcp] Existing backend at ${BACKEND_BASE_URL} failed health checks; stop it or choose a new LOCAL_BACKEND_PORT." >&2
    else
      echo "[index-mcp] Backend failed to become ready at ${BACKEND_BASE_URL}; the port may already be in use. Stop the conflicting process or set LOCAL_BACKEND_PORT." >&2
    fi
    exit 1
  fi
else
  # Fallback: allow backend a moment to boot if curl is unavailable
  sleep 1
fi

echo "[index-mcp] Launching MCP server from $DIST_ENTRY" >&2
"$NODE_BIN" "$DIST_ENTRY"
