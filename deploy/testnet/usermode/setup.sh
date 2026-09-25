#!/usr/bin/env bash
# Prepares N testnet nodes under ~/boltchain on one host, without root. Idempotent: keys and
# data are created once and kept.
#
#   setup.sh <nodes> <keys-per-node> <mining-threads-per-node> <public-host>
#
# Node 1 is the public one: P2P on 8017, JSON-RPC on 0.0.0.0:8545, IPFS gateway on 0.0.0.0:8080.
# Node i (i > 1) uses P2P 8017 + 10(i-1), RPC and metrics on localhost only.
set -euo pipefail
N=$1; KEYS=$2; THREADS=$3; PUBLIC=$4
ROOT=$HOME/boltchain
B=$ROOT/bin/boltchain
chmod +x "$ROOT/bin/boltchain" "$ROOT/node.sh" "$ROOT/start.sh" "$ROOT/stop.sh"
for i in $(seq 1 "$N"); do
  D=$ROOT/n$i
  mkdir -p "$D/keys" && chmod 700 "$D" "$D/keys"
  [ -f "$D/wallet.json" ] || "$B" wallet new --out "$D/wallet.json" >/dev/null
  for k in $(seq 1 "$KEYS"); do
    [ -f "$D/keys/v$k.json" ] || "$B" keys new --out "$D/keys/v$k.json" >/dev/null
  done
  "$B" node-id --node-key "$D/data/node.key" > "$D/peer-id"
done
BOOT1="/ip4/$PUBLIC/udp/8017/quic-v1/p2p/$(cat "$ROOT/n1/peer-id") /ip4/$PUBLIC/tcp/8017/p2p/$(cat "$ROOT/n1/peer-id")"
for i in $(seq 1 "$N"); do
  D=$ROOT/n$i
  P2P=$((8017 + 10 * (i - 1)))
  ADDR=$(grep -o '"address": *"0x[0-9a-fA-F]*"' "$D/wallet.json" | grep -o '0x[0-9a-fA-F]*')
  if [ "$i" = 1 ]; then RPC=0.0.0.0:8545; else RPC=127.0.0.1:$((8545 + i - 1)); fi
  A="--genesis $ROOT/testnet.json --datadir $D/data --rpc $RPC --p2p-port $P2P"
  A+=" --metrics 127.0.0.1:$((9016 + i)) --mine --randomx-fast --mining-threads $THREADS"
  A+=" --beneficiary $ADDR --extra-data cafeca-n$i"
  [ "$i" = 1 ] && A+=" --gateway 0.0.0.0:8080 --explorer --archive --relay-server"
  # Contract names for the explorer (e.g. scripts/uniswap-v4/deployments/8018.json copied here).
  for l in "$ROOT"/labels/*.json; do [ "$i" = 1 ] && [ -f "$l" ] && A+=" --explorer-labels $l"; done
  for k in "$D"/keys/v*.json; do A+=" --key $k"; done
  # Everyone dials node 1 and the other local nodes (as public nodes would).
  for b in $BOOT1; do [ "$i" != 1 ] && A+=" --bootnode $b"; done
  for j in $(seq 2 "$N"); do
    [ "$j" = "$i" ] && continue
    A+=" --bootnode /ip4/127.0.0.1/tcp/$((8017 + 10 * (j - 1)))/p2p/$(cat "$ROOT/n$j/peer-id")"
  done
  echo "$A" > "$D/args"
done
# Start again after a reboot (no root needed).
{ { crontab -l 2>/dev/null || true; } | { grep -v 'boltchain/start.sh' || true; }; echo "@reboot $ROOT/start.sh $N"; } | crontab - \
  || echo "warning: could not install the @reboot crontab entry"
echo "bootnodes: $BOOT1"
