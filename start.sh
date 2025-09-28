#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

DIST_ENTRY="$SCRIPT_DIR/dist/server.js"
NODE_BIN="$(command -v node || true)"
if [[ -z "$NODE_BIN" ]]; then
  echo "[index-mcp] Error: node is not on PATH. Install Node.js 18+ and try again." >&2
  exit 1
fi

if [[ ! -f "$DIST_ENTRY" ]]; then
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

echo "[index-mcp] Launching MCP server from $DIST_ENTRY" >&2
exec "$NODE_BIN" "$DIST_ENTRY"
