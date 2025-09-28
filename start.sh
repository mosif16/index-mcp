#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if ! command -v cargo >/dev/null 2>&1; then
  echo "[index-mcp] Error: cargo is not on PATH. Install Rust (https://www.rust-lang.org/tools/install) and try again." >&2
  exit 1
fi

PROFILE="${INDEX_MCP_BUILD_PROFILE:-release}"
CARGO_ARGS=(--manifest-path "$SCRIPT_DIR/Cargo.toml")

case "$PROFILE" in
  release)
    CARGO_ARGS+=(--release)
    BIN_PATH="$SCRIPT_DIR/target/release/index-mcp"
    ;;
  debug)
    BIN_PATH="$SCRIPT_DIR/target/debug/index-mcp"
    ;;
  *)
    CARGO_ARGS+=(--profile "$PROFILE")
    BIN_PATH="$SCRIPT_DIR/target/$PROFILE/index-mcp"
    ;;
esac

if [[ "${INDEX_MCP_SKIP_BUILD:-0}" != "1" ]]; then
  echo "[index-mcp] Building Rust server (profile: $PROFILE)..." >&2
  if [[ -n "${INDEX_MCP_CARGO_FLAGS:-}" ]]; then
    # shellcheck disable=SC2206
    EXTRA_FLAGS=(${INDEX_MCP_CARGO_FLAGS})
  else
    EXTRA_FLAGS=()
  fi
  cargo build "${CARGO_ARGS[@]}" "${EXTRA_FLAGS[@]}" >&2
fi

if [[ ! -x "$BIN_PATH" ]]; then
  echo "[index-mcp] Error: compiled binary not found at $BIN_PATH" >&2
  echo "[index-mcp] Hint: ensure the build step above succeeded." >&2
  exit 1
fi

exec "$BIN_PATH" "$@"
