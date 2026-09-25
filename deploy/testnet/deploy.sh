#!/usr/bin/env bash
# Deploys or upgrades the testnet on the hosts in hosts.txt (see hosts.txt.example).
#
#   deploy.sh <hosts.txt> <dist-dir> [ssh-key]
#
# dist-dir holds boltchain-linux-x86_64 and/or boltchain-linux-aarch64 and testnet.json.
# The first run starts every node, collects peer ids and then restarts all nodes with the full
# bootnode list; later runs upgrade binaries in place (keys and data are kept).
set -euo pipefail
HOSTS=$1
DIST=$2
KEY=${3:-}
HERE=$(cd "$(dirname "$0")" && pwd)
SSH_OPTS=(-o StrictHostKeyChecking=accept-new -o ConnectTimeout=15)
[ -n "$KEY" ] && SSH_OPTS+=(-i "$KEY")
ssh_() { ssh "${SSH_OPTS[@]}" "$@"; }
scp_() { scp -q "${SSH_OPTS[@]}" "$@"; }
sudo_() { local t=$1; shift; if [[ $t == root@* ]]; then ssh_ "$t" "$@"; else ssh_ "$t" sudo "$@"; fi; }

multiaddrs=()
while read -r name target public threads keys rpc fast; do
  [[ -z ${name:-} || $name == \#* ]] && continue
  arch=$(ssh_ "$target" uname -m)
  case $arch in x86_64) bin=boltchain-linux-x86_64 ;; aarch64|arm64) bin=boltchain-linux-aarch64 ;; *) echo "$name: unsupported $arch"; exit 1 ;; esac
  echo "== $name ($target, $arch)"
  ssh_ "$target" "mkdir -p /tmp/boltchain-deploy"
  scp_ "$DIST/$bin" "$target:/tmp/boltchain-deploy/boltchain"
  scp_ "$DIST/testnet.json" "$HERE/install.sh" "$HERE/boltchain.service" "$target:/tmp/boltchain-deploy/"
  out=$(sudo_ "$target" bash /tmp/boltchain-deploy/install.sh "$threads" "$keys" "$rpc" "$fast")
  echo "   $out"
  peer=$(echo "$out" | awk '/^peer/{print $2}')
  if [[ $public =~ ^[0-9.]+$ ]]; then proto=ip4; else proto=dns4; fi
  multiaddrs+=("/$proto/$public/udp/8017/quic-v1/p2p/$peer" "/$proto/$public/tcp/8017/p2p/$peer")
done < "$HOSTS"

printf '%s\n' "${multiaddrs[@]}" > "$HERE/bootnodes.txt"
echo "== bootnodes (saved to bootnodes.txt)"
cat "$HERE/bootnodes.txt"
while read -r name target public threads keys rpc fast; do
  [[ -z ${name:-} || $name == \#* ]] && continue
  own=$(sudo_ "$target" cat /etc/boltchain/peer-id 2>/dev/null || true)
  grep -v "${own:-none}" "$HERE/bootnodes.txt" | ssh_ "$target" "cat > /tmp/boltchain-deploy/bootnodes"
  sudo_ "$target" "install -m 644 -o boltchain /tmp/boltchain-deploy/bootnodes /etc/boltchain/bootnodes"
  sudo_ "$target" bash /tmp/boltchain-deploy/install.sh "$threads" "$keys" "$rpc" "$fast" >/dev/null
  echo "restarted $name with $(grep -vc "${own:-none}" "$HERE/bootnodes.txt") bootnodes"
done < "$HOSTS"
