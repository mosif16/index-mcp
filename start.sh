#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

CARGO_BIN="$(command -v cargo || true)"
if [[ -z "$CARGO_BIN" ]]; then
  echo "[index-mcp] Error: cargo is not on PATH. Install the Rust toolchain and try again." >&2
  exit 1
fi

MODE_ARG=""
if [[ $# -gt 0 ]]; then
  case "$1" in
    prod|production|dev|development)
      MODE_ARG="$1"
      shift
      ;;
  esac
fi

MODE="${MODE_ARG:-${INDEX_MCP_MODE:-production}}"

PROFILE_DEFAULT="release"
DEFAULT_ARGS=()

case "$MODE" in
  prod|production)
    MODE="production"
    PROFILE_DEFAULT="release"
    DEFAULT_ARGS=()
    ;;
  dev|development)
    MODE="development"
    PROFILE_DEFAULT="debug"
    DEFAULT_ARGS=(--watch --watch-no-initial)
    ;;
  *)
    echo "[index-mcp] Warning: unknown mode '$MODE'; defaulting to production" >&2
    MODE="production"
    PROFILE_DEFAULT="release"
    DEFAULT_ARGS=()
    ;;
esac

PROFILE="${INDEX_MCP_CARGO_PROFILE:-$PROFILE_DEFAULT}"

# shellcheck disable=SC2206
if [[ -n "${INDEX_MCP_ARGS:-}" ]]; then
  ENV_ARGS=( ${INDEX_MCP_ARGS} )
else
  ENV_ARGS=()
fi

SERVER_ARGS=()
if [[ ${#DEFAULT_ARGS[@]} -gt 0 ]]; then
  SERVER_ARGS+=("${DEFAULT_ARGS[@]}")
fi
if [[ ${#ENV_ARGS[@]} -gt 0 ]]; then
  SERVER_ARGS+=("${ENV_ARGS[@]}")
fi
if [[ $# -gt 0 ]]; then
  SERVER_ARGS+=("$@")
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

if [[ ${#SERVER_ARGS[@]} -gt 0 ]]; then
  CMD+=(-- "${SERVER_ARGS[@]}")
fi

echo "[index-mcp] Launching Rust MCP server (mode: $MODE, profile: $PROFILE)" >&2
cd "$SCRIPT_DIR"
exec "${CMD[@]}"
