#!/usr/bin/env bash
# Stops all nodes (their supervisors first, so they are not restarted).
for s in $(screen -ls | grep -o '[0-9]*\.bolt-n[0-9]*'); do screen -S "$s" -X quit; done
pkill -INT -u "$(id -u)" -f 'boltchain/bin/boltchain validator' || true
sleep 3
pkill -KILL -u "$(id -u)" -f 'boltchain/bin/boltchain validator' || true
echo stopped
