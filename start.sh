#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

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

CMD=("$CARGO_BIN" run --manifest-path "$SCRIPT_DIR/Cargo.toml" -p index-mcp-server --bin index-mcp-server)

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
