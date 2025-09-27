#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
cd "$SCRIPT_DIR"

DIST_ENTRY="dist/server.js"
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
  npm run build >&2
fi

echo "[index-mcp] Launching MCP server from $DIST_ENTRY" >&2
exec "$NODE_BIN" "$DIST_ENTRY"
