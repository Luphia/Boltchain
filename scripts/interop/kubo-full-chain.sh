#!/usr/bin/env bash
# Downloads a Boltchain chain with Kubo (the reference IPFS implementation) only: starting from
# the head block's IPFS root (`ipfsRoot` in eth_getBlockByNumber), `ipfs dag get` decodes each
# dag-cbor envelope, and `ipfs block get` fetches its header and body chunks over Bitswap.
# The header's CID digest is checked against the block hash from RPC, and body bytes are counted.
#
#   IPFS_PATH=... kubo-full-chain.sh <rpc-url> [max-blocks]
# The Kubo daemon must already be connected to a Boltchain node (`ipfs swarm connect ...`).
set -euo pipefail
RPC=$1; MAX=${2:-100000}
IPFS=${IPFS:-ipfs}
rpc() { curl -s -X POST -H 'content-type: application/json' --data "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"$1\",\"params\":$2}" "$RPC"; }
cid=$(rpc eth_getBlockByNumber '["latest",false]' | python3 -c 'import json,sys;print(json.load(sys.stdin)["result"]["ipfsRoot"])')
blocks=0; chunks=0; bytes=0; bad=0; start=$(date +%s)
while [ -n "$cid" ] && [ "$blocks" -lt "$MAX" ]; do
  env=$(timeout 30 $IPFS dag get "$cid")
  read -r height header parent nchunks < <(echo "$env" | python3 -c '
import json,sys
e=json.load(sys.stdin)
print(e["height"], e["header"]["/"], (e.get("parent") or {}).get("/","none"), " ".join(c["/"] for c in e["chunks"]) or "-")')
  hdr_len=$(timeout 30 $IPFS block get "$header" | wc -c)
  for c in $nchunks; do
    [ "$c" = "-" ] && continue
    n=$(timeout 30 $IPFS block get "$c" | wc -c); chunks=$((chunks+1)); bytes=$((bytes+n))
  done
  # The header CID's multihash digest is the block hash.
  digest=$($IPFS cid format -f '%D' -b base16 "$header" 2>/dev/null || true)
  want=$(rpc eth_getBlockByNumber "[\"0x$(printf %x "$height")\",false]" | python3 -c 'import json,sys;print(json.load(sys.stdin)["result"]["hash"][2:])')
  [ "${digest#f}" = "$want" ] || [ "$digest" = "$want" ] || { bad=$((bad+1)); echo "block $height: header digest $digest != rpc $want"; }
  blocks=$((blocks+1))
  [ $((height % 50)) -eq 0 ] && echo "block $height ok (header $hdr_len bytes)"
  [ "$parent" = none ] && break
  cid=$parent
done
echo "{\"blocks\":$blocks,\"chunks\":$chunks,\"bodyBytes\":$bytes,\"mismatches\":$bad,\"seconds\":$(( $(date +%s) - start ))}"
