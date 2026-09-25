#!/usr/bin/env bash
# On every host: stake each validator key with <amount> BOLT from the host's mining account and
# publish the node as its history peer. Run once the accounts have mined enough.
#
#   stake.sh <hosts.txt> <amount-per-key> [ssh-key]
set -euo pipefail
HOSTS=$1; AMOUNT=$2; KEY=${3:-}
SSH_OPTS=(-o StrictHostKeyChecking=accept-new -o ConnectTimeout=15)
[ -n "$KEY" ] && SSH_OPTS+=(-i "$KEY")
while read -r name target _rest; do
  [[ -z ${name:-} || $name == \#* ]] && continue
  echo "== $name"
  pre=""; [[ $target == root@* ]] || pre="sudo"
  ssh "${SSH_OPTS[@]}" "$target" "$pre bash -c '
    B=/opt/boltchain/boltchain
    \$B wallet balance --wallet /etc/boltchain/wallet.json
    for k in /etc/boltchain/keys/v*.json; do
      \$B wallet stake --wallet /etc/boltchain/wallet.json --key \$k --amount $AMOUNT || true
      \$B wallet set-peer --wallet /etc/boltchain/wallet.json --key \$k --node-key /var/lib/boltchain/node.key || true
    done'"
done < "$HOSTS"
