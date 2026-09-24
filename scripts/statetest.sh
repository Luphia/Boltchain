#!/usr/bin/env bash
# Run the Ethereum execution-spec-tests state tests against revm (the EVM Boltchain embeds).
# M0 acceptance: every Osaka state test passes.
#
# Usage: scripts/statetest.sh [fixtures-dir]
#   FIXTURES_URL  override the fixtures tarball (default: latest stable EEST release)
#   REVME_VERSION revme version to install (must match the workspace revm major version)
set -euo pipefail

REVME_VERSION="${REVME_VERSION:-43.0.3}"
FIXTURES_URL="${FIXTURES_URL:-https://github.com/ethereum/execution-spec-tests/releases/latest/download/fixtures_stable.tar.gz}"
WORK="${1:-target/eest}"

if ! command -v revme >/dev/null || ! revme --version | grep -q "$REVME_VERSION"; then
  cargo install revme --version "$REVME_VERSION" --locked
fi

if [ ! -d "$WORK/fixtures" ]; then
  mkdir -p "$WORK"
  echo "downloading $FIXTURES_URL"
  curl -fsSL "$FIXTURES_URL" | tar -xz -C "$WORK"
fi

STATE_DIR="$(find "$WORK" -type d -name state_tests | head -n1)"
[ -n "$STATE_DIR" ] || { echo "state_tests directory not found in $WORK" >&2; exit 1; }

OSAKA_DIR="$(find "$STATE_DIR" -maxdepth 1 -type d -iname osaka | head -n1)"
if [ -n "$OSAKA_DIR" ]; then
  echo "== Osaka-introduced tests: $OSAKA_DIR"
  revme statetest --omit-progress "$OSAKA_DIR"
fi

echo "== full state test suite (all forks up to Osaka): $STATE_DIR"
revme statetest --omit-progress "$STATE_DIR"
