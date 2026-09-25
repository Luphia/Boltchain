#!/usr/bin/env bash
# Starts nodes 1..N in detached screen sessions (bolt-n<i>); already running ones are left alone.
N=${1:-$(ls -d "$HOME"/boltchain/n* 2>/dev/null | wc -l)}
for i in $(seq 1 "$N"); do
  if screen -ls | grep -q "\.bolt-n$i\b"; then echo "n$i running"; continue; fi
  screen -dmS "bolt-n$i" "$HOME/boltchain/node.sh" "$i"
  echo "n$i started"
done
