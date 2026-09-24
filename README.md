# Boltchain

一條以 IPFS 同步與保存區塊、支援 EVM Osaka 的輕量公開 PoS 鏈。
驗證者節點的目標硬體是樹莓派級機器：4 核 ARM、4 GB RAM、128 GB SSD。

- 開發方案：<https://claude.ai/code/artifact/cd491a00-a0a2-4e92-92f3-205aa77d55c5>
- 設計決策紀錄：[`docs/adr/`](docs/adr/)
- 里程碑進度：[`docs/milestones.md`](docs/milestones.md)

## 目前狀態：M0 骨架

| 項目 | 狀態 |
| --- | --- |
| Cargo workspace，含 11 個 crate | 完成（`primitives`、`exec` 已實作，其餘為骨架） |
| 鏈參數（chain id 8017、6 s slot、512 席、BOLT 發行曲線） | 完成：`crates/primitives/src/params.rs` |
| Genesis 格式、驗證、genesis header 與 state root | 完成，並以獨立的 Python 實作交叉驗證 |
| revm Osaka 執行（CLZ、P256VERIFY、EIP-7825、拒收 blob） | 完成：`crates/exec` |
| Osaka state tests | 腳本完成，在 CI 上執行 |
| CI（x86_64 + ARM64） | 完成：`.github/workflows/ci.yml` |

## 快速開始

```sh
cargo test --workspace
cargo run -p boltchain -- params
cargo run -p boltchain -- genesis inspect genesis/devnet.json
scripts/statetest.sh        # 需要能連到 github.com 下載 fixtures
```

## Workspace 結構

```
crates/
  primitives/  鏈參數、genesis 格式
  ipld/        IPLD 編碼、CAR          (M2)
  store/       libmdbx 儲存層           (M1)
  exec/        revm Osaka 執行
  consensus/   HotStuff-2、BLS-VRF 委員會 (M3)
  net/         libp2p、gossipsub、bitswap (M2)
  sync/        同步與快照               (M5)
  txpool/      交易池                   (M1)
  rpc/         eth_* JSON-RPC          (M1)
  sim/         決定性模擬器             (M3)
  node/        `boltchain` 執行檔
contracts/     系統合約（Foundry，M4）
genesis/       genesis 檔
```
