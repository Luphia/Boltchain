#!/usr/bin/env bash
# Runs on 211.22.118.148. From block START (default 3300) gathers this host's BOLT in the n1
# wallet (211.22.118.149 sends its BOLT there too, fund-host2.sh), waits until it covers the
# missing PoS stake (100,000 BOLT in total; 20,800 already staked), then stakes the 8 local
# validator keys evenly from the n1 wallet and publishes each validator's history peer.
set -u
START=${START:-3300}
NEED=${NEED:-79200}
B=$HOME/boltchain/bin/boltchain
R=http://127.0.0.1:18545
W=$HOME/boltchain/n1/wallet.json
head() { curl -s -m5 -X POST -H content-type:application/json --data '{"jsonrpc":"2.0","id":1,"method":"eth_blockNumber","params":[]}' $R | python3 -c 'import json,sys;print(int(json.load(sys.stdin)["result"],16))'; }
bal() { $B wallet balance --wallet $1 --rpc $R | awk '{print $2}'; }
while [ "$(head 2>/dev/null || echo 0)" -lt "$START" ]; do sleep 30; done
echo "$(date -Is) block $(head): gathering"
B2=$(bal $HOME/boltchain/n2/wallet.json)
A2=$(python3 -c "import math;print(max(0, math.floor(float('$B2') - 5)))")
TO=$(python3 -c "import json;print(json.load(open('$W'))['address'])")
[ "$A2" -gt 0 ] && $B wallet send --wallet $HOME/boltchain/n2/wallet.json --to $TO --value $A2 --rpc $R
while :; do
  BAL=$(bal $W)
  OK=$(python3 -c "print(1 if float('$BAL') - 60 >= $NEED else 0)")
  echo "$(date -Is) block $(head): balance $BAL"
  [ "$OK" = 1 ] && break
  sleep 30
done
EACH=$(python3 -c "import math;print(math.floor((float('$BAL') - 60) / 8))")
echo "staking 8 validators with $EACH BOLT each"
for i in 1 2; do
  for k in 1 2 3 4; do
    K=$HOME/boltchain/n$i/keys/v$k.json
    $B wallet stake --wallet $W --key $K --amount $EACH --rpc $R
    $B wallet set-peer --wallet $W --key $K --node-key $HOME/boltchain/n$i/data/node.key --rpc $R
  done
done
echo "$(date -Is) done"
