#!/usr/bin/env bash
# Supervises one node without root: restarts it when it exits and rotates its log at 200 MB.
# Normally started by start.sh inside a detached `screen` session.
#
#   node.sh <index>          (reads ~/boltchain/n<index>/args)
set -uo pipefail
I=$1
ROOT=$HOME/boltchain
DIR=$ROOT/n$I
LOG=$DIR/node.log
export RUST_LOG=${RUST_LOG:-info,libmdbx=warn,libp2p=warn}
while true; do
  if [ -f "$LOG" ] && [ "$(stat -c %s "$LOG")" -gt $((200 * 1024 * 1024)) ]; then
    mv -f "$LOG" "$LOG.1"
  fi
  # shellcheck disable=SC2046
  "$ROOT/bin/boltchain" validator $(cat "$DIR/args") >> "$LOG" 2>&1
  echo "$(date -Is) node exited with status $?; restarting in 5 s" >> "$LOG"
  sleep 5
done
