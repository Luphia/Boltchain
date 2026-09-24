# Boltchain

一條以 IPFS 同步與保存區塊、支援 EVM Osaka 的輕量公開 PoS 鏈。
驗證者節點的目標硬體是樹莓派級機器：4 核 ARM、4 GB RAM、128 GB SSD。

- 開發方案：<https://claude.ai/code/artifact/cd491a00-a0a2-4e92-92f3-205aa77d55c5>
- 設計決策紀錄：[`docs/adr/`](docs/adr/)
- 里程碑進度：[`docs/milestones.md`](docs/milestones.md)

## 目前狀態：M1 單節點鏈

| 項目 | 狀態 |
| --- | --- |
| 鏈參數、genesis 格式與驗證（M0） | 完成 |
| revm Osaka 執行、區塊執行器（EIP-4788 / EIP-2935 system call、收據、EIP-1559、EIP-7934） | 完成：`crates/exec` |
| libmdbx 儲存層、路徑式增量 MPT、最近 128 塊的狀態歷史 | 完成：`crates/store` |
| 出塊與匯入（匯入端會重新執行並逐欄比對 header） | 完成：`crates/chain` |
| 交易池 | 完成：`crates/txpool` |
| `eth_*` JSON-RPC（含 CORS） | 完成：`crates/rpc` |
| 單一出塊者 devnet | 完成：`boltchain devnet` |
| 基準測試（轉帳、BN254 / BLS12-381 pairing 最壞情況區塊） | 完成：`boltchain bench` |
| Osaka state tests | 腳本完成，在 CI 上執行 |

## 快速開始

```sh
cargo test --workspace
cargo run --release -p boltchain -- devnet          # JSON-RPC 在 http://127.0.0.1:8545
cargo run --release -p boltchain -- bench           # 區塊執行基準
cargo run -p boltchain -- genesis inspect genesis/devnet.json
scripts/statetest.sh                                 # 需要能連到 github.com
```

devnet 的使用方式（MetaMask、Foundry）見 [`docs/devnet.md`](docs/devnet.md)。

## Workspace 結構

```
crates/
  primitives/  鏈參數、genesis 格式
  exec/        revm Osaka 執行、區塊執行器
  store/       libmdbx 儲存層、路徑式增量 MPT、狀態歷史
  chain/       genesis 初始化、出塊與匯入
  txpool/      交易池
  rpc/         eth_* JSON-RPC
  node/        `boltchain` 執行檔：devnet、bench、genesis 工具
  ipld/        IPLD 編碼、CAR          (M2)
  net/         libp2p、gossipsub、bitswap (M2)
  consensus/   HotStuff-2、BLS-VRF 委員會 (M3)
  sim/         決定性模擬器             (M3)
  sync/        同步與快照               (M5)
contracts/     系統合約（Foundry，M4）
genesis/       genesis 檔（devnet.json：主網格式；dev.json：本地開發鏈）
```

## 授權

MIT，見 [`LICENSE`](LICENSE)。
