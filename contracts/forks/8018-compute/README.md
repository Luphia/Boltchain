# Testnet fork `compute` (chain 8018, block 6001)

`ComputeMarket.bin` is the runtime code the fork installs at `0xB017…0006`. It is a frozen copy of
`out/ComputeMarket.json` at the time the fork was scheduled: nodes replaying block 6001 must install
exactly these bytes, whatever the current source says. A later change to `ComputeMarket` on the
testnet needs a new fork with its own copy.
