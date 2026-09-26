#!/usr/bin/env bash
# Runs on 211.22.118.149. From block START (default 3300) sends each local node wallet's BOLT,
# keeping 30 for gas, to the staking wallet on 211.22.118.148, which stakes 8 more validators
# so the testnet reaches PoS (16 validators, 100,000 BOLT).
set -u
START=${START:-3300}
TO=0xee893ae843ec50f290141367953afdbc2721d3ab
B=$HOME/boltchain/bin/boltchain
R=http://127.0.0.1:8545
head() { curl -s -m5 -X POST -H content-type:application/json --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' $R | python3 -c 'import json,sys;print(int(json.load(sys.stdin)["result"],16))'; }
while [ "$(head 2>/dev/null || echo 0)" -lt "$START" ]; do sleep 30; done
echo "$(date -Is) block $(head): funding $TO"
for i in 1 2 3 4; do
  W=$HOME/boltchain/n$i/wallet.json
  BAL=$($B wallet balance --wallet $W --rpc $R | awk '{print $2}')
  AMT=$(python3 -c "import math;print(max(0, math.floor(float('$BAL') - 30)))")
  echo "n$i balance $BAL, sending $AMT"
  [ "$AMT" -gt 0 ] && $B wallet send --wallet $W --to $TO --value $AMT --rpc $R
done
echo "$(date -Is) done"
