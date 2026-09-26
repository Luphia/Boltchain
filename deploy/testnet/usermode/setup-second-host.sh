#!/usr/bin/env bash
# Prepares N testnet nodes under ~/boltchain on a second host (211.22.118.148), without root.
# Idempotent: keys and data are created once and kept. RPC (18545, 18546, …) and metrics (19117, …) on localhost only (8545 is taken on this host).
#
#   setup.sh <nodes> <keys-per-node> <mining-threads-per-node> <public-host> <tag>
set -euo pipefail
N=$1; KEYS=$2; THREADS=$3; PUBLIC=$4; TAG=$5
ROOT=$HOME/boltchain
B=$ROOT/bin/boltchain
BOOT="/ip4/211.22.118.149/udp/8017/quic-v1/p2p/12D3KooWCFZiVi5pkzNA83AZqaUevCbi5fYULiEGtmz8FSvMuhDo /ip4/211.22.118.149/tcp/8017/p2p/12D3KooWCFZiVi5pkzNA83AZqaUevCbi5fYULiEGtmz8FSvMuhDo"
chmod +x "$B" "$ROOT"/*.sh
for i in $(seq 1 "$N"); do
  D=$ROOT/n$i
  mkdir -p "$D/keys" && chmod 700 "$D" "$D/keys"
  [ -f "$D/wallet.json" ] || "$B" wallet new --out "$D/wallet.json" >/dev/null
  for k in $(seq 1 "$KEYS"); do
    [ -f "$D/keys/v$k.json" ] || "$B" keys new --out "$D/keys/v$k.json" >/dev/null
  done
  "$B" node-id --node-key "$D/data/node.key" > "$D/peer-id"
done
for i in $(seq 1 "$N"); do
  D=$ROOT/n$i
  P2P=$((8017 + 10 * (i - 1)))
  ADDR=$(grep -o '"address": *"0x[0-9a-fA-F]*"' "$D/wallet.json" | grep -o '0x[0-9a-fA-F]*')
  A="--genesis $ROOT/testnet.json --datadir $D/data --rpc 127.0.0.1:$((18545 + i - 1)) --p2p-port $P2P"
  A+=" --metrics 127.0.0.1:$((19116 + i)) --mine --randomx-fast --mining-threads $THREADS"
  A+=" --beneficiary $ADDR --extra-data $TAG-n$i"
  for k in "$D"/keys/v*.json; do A+=" --key $k"; done
  for b in $BOOT; do A+=" --bootnode $b"; done
  for j in $(seq 1 "$N"); do
    [ "$j" = "$i" ] && continue
    A+=" --bootnode /ip4/127.0.0.1/tcp/$((8017 + 10 * (j - 1)))/p2p/$(cat "$ROOT/n$j/peer-id")"
  done
  echo "$A" > "$D/args"
done
{ { crontab -l 2>/dev/null || true; } | { grep -v 'boltchain/start.sh' || true; }; echo "@reboot $ROOT/start.sh $N"; } | crontab - \
  || echo "warning: could not install the @reboot crontab entry"
echo "public host $PUBLIC; peers:"; for i in $(seq 1 "$N"); do echo "n$i $(cat $ROOT/n$i/peer-id)"; done
