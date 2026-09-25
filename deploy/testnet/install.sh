#!/usr/bin/env bash
# Runs on a testnet host as root (deploy.sh uploads it with the binary and genesis to
# /tmp/boltchain-deploy). Idempotent: keys are created once and never overwritten.
#
#   install.sh <mining-threads> <validator-keys> <public-rpc 0|1> <randomx-fast 0|1>
set -euo pipefail
THREADS=${1:-1}
KEYS=${2:-2}
PUBLIC_RPC=${3:-0}
FAST=${4:-0}
SRC=/tmp/boltchain-deploy
BIN=/opt/boltchain/boltchain

id boltchain >/dev/null 2>&1 || useradd --system --home-dir /var/lib/boltchain --shell /usr/sbin/nologin boltchain
install -d -m 755 /opt/boltchain
install -d -m 750 -o boltchain -g boltchain /var/lib/boltchain
install -d -m 700 -o boltchain -g boltchain /etc/boltchain /etc/boltchain/keys
install -m 755 "$SRC/boltchain" "$BIN.new" && mv -f "$BIN.new" "$BIN"
install -m 644 "$SRC/testnet.json" /opt/boltchain/testnet.json
install -m 644 "$SRC/boltchain.service" /etc/systemd/system/boltchain.service

# Keys stay on this host: an account (mining rewards, stake owner), validator keys, node identity.
[ -f /etc/boltchain/wallet.json ] || "$BIN" wallet new --out /etc/boltchain/wallet.json >/dev/null
for i in $(seq 1 "$KEYS"); do
  [ -f "/etc/boltchain/keys/v$i.json" ] || "$BIN" keys new --out "/etc/boltchain/keys/v$i.json" >/dev/null
done
"$BIN" node-id --node-key /var/lib/boltchain/node.key > /etc/boltchain/peer-id
chown -R boltchain:boltchain /etc/boltchain /var/lib/boltchain

ADDR=$(python3 -c 'import json;print(json.load(open("/etc/boltchain/wallet.json"))["address"])' 2>/dev/null \
  || grep -o '"address": *"0x[0-9a-fA-F]*"' /etc/boltchain/wallet.json | grep -o '0x[0-9a-fA-F]*')
RPC=127.0.0.1:8545
[ "$PUBLIC_RPC" = 1 ] && RPC=0.0.0.0:8545
ARGS="--genesis /opt/boltchain/testnet.json --datadir /var/lib/boltchain --rpc $RPC --p2p-port 8017"
ARGS+=" --metrics 127.0.0.1:9017 --gateway 0.0.0.0:8080"
ARGS+=" --mine --mining-threads $THREADS --beneficiary $ADDR --extra-data $(hostname -s | cut -c1-24)"
[ "$FAST" = 1 ] && ARGS+=" --randomx-fast"
for k in /etc/boltchain/keys/v*.json; do ARGS+=" --key $k"; done
if [ -f /etc/boltchain/bootnodes ]; then
  while read -r b; do [ -n "$b" ] && ARGS+=" --bootnode $b"; done < /etc/boltchain/bootnodes
fi
echo "BOLT_ARGS=\"$ARGS\"" > /etc/boltchain/boltchain.env
chown boltchain:boltchain /etc/boltchain/boltchain.env

# Open the P2P port if a firewall is active.
if command -v ufw >/dev/null && ufw status | grep -q active; then
  ufw allow 8017 >/dev/null; ufw allow 8080/tcp >/dev/null
  [ "$PUBLIC_RPC" = 1 ] && ufw allow 8545/tcp >/dev/null
fi

systemctl daemon-reload
systemctl enable boltchain >/dev/null 2>&1
systemctl restart boltchain
echo "peer $(cat /etc/boltchain/peer-id) account $ADDR"
