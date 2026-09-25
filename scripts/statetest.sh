#!/usr/bin/env bash
# Run the Ethereum execution-spec-tests state tests against revm (the EVM Boltchain embeds).
# M0 acceptance: every Osaka state test passes.
#
# Usage: scripts/statetest.sh [fixtures-dir]
#   FIXTURES_URL  override the fixtures tarball (default: EEST v5.4.0, the release M0 passed with)
#   (default fixtures dir: ~/.cache/boltchain/eest-v5.4.0, outside target/ so build caches that
#   prune target/ cannot leave a hollow copy behind)
#   REVME_VERSION revme version to install (must match the workspace revm major version)
set -euo pipefail

REVME_VERSION="${REVME_VERSION:-43.0.3}"
FIXTURES_URL="${FIXTURES_URL:-https://github.com/ethereum/execution-spec-tests/releases/download/v5.4.0/fixtures_stable.tar.gz}"
WORK="${1:-$HOME/.cache/boltchain/eest-v5.4.0}"

# revme has no --version flag: ask cargo which version is installed.
if ! command -v revme >/dev/null || ! cargo install --list | grep -q "^revme v$REVME_VERSION:"; then
  cargo install revme --version "$REVME_VERSION" --locked --force
fi

# Require actual test files, not just the directory.
if [ -z "$(find "$WORK" -name '*.json' -path '*state_tests*' -print -quit 2>/dev/null)" ]; then
  rm -rf "$WORK"
  mkdir -p "$WORK"
  echo "downloading $FIXTURES_URL"
  curl -fsSL --retry 5 --retry-delay 10 --retry-all-errors -o "$WORK/fixtures.tar.gz" "$FIXTURES_URL"
  tar -xzf "$WORK/fixtures.tar.gz" -C "$WORK"
  rm "$WORK/fixtures.tar.gz"
fi

# The top-level fixtures/state_tests (blockchain_tests/static/state_tests holds blockchain-format
# files that `revme statetest` cannot read).
STATE_DIR="$(find "$WORK" -maxdepth 2 -type d -path '*/fixtures/state_tests' | head -n1)"
[ -n "$STATE_DIR" ] || { echo "state_tests directory not found in $WORK" >&2; exit 1; }

OSAKA_DIR="$(find "$STATE_DIR" -maxdepth 1 -type d -iname osaka | head -n1)"
if [ -n "$OSAKA_DIR" ]; then
  echo "== Osaka-introduced tests: $OSAKA_DIR"
  revme statetest --omit-progress "$OSAKA_DIR"
fi

echo "== full state test suite (all forks up to Osaka): $STATE_DIR"
revme statetest --omit-progress "$STATE_DIR"
