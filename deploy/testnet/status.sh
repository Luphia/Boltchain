#!/usr/bin/env bash
# Prints every node's status (from its /health endpoint) and the last log lines; with --logs DIR
# also saves the last hour of logs of each node into DIR/<name>.log.
#
#   status.sh <hosts.txt> [ssh-key] [--logs DIR]
set -euo pipefail
HOSTS=$1; shift
KEY=""; LOGS=""
while [ $# -gt 0 ]; do
  case $1 in --logs) LOGS=$2; shift 2 ;; *) KEY=$1; shift ;; esac
done
SSH_OPTS=(-o StrictHostKeyChecking=accept-new -o ConnectTimeout=10)
[ -n "$KEY" ] && SSH_OPTS+=(-i "$KEY")
[ -n "$LOGS" ] && mkdir -p "$LOGS"
while read -r name target _rest; do
  [[ -z ${name:-} || $name == \#* ]] && continue
  health=$(ssh "${SSH_OPTS[@]}" "$target" curl -s --max-time 5 127.0.0.1:9017/health 2>/dev/null || echo unreachable)
  echo "$name $health"
  if [ -n "$LOGS" ]; then
    ssh "${SSH_OPTS[@]}" "$target" "journalctl -u boltchain --since '1 hour ago' --no-pager -o short-iso" > "$LOGS/$name.log" 2>/dev/null || true
    echo "   $(grep -c ' WARN ' "$LOGS/$name.log" || true) warnings, $(grep -c ' ERROR ' "$LOGS/$name.log" || true) errors in the last hour"
  fi
done < "$HOSTS"
