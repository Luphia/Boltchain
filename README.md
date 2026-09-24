# Boltchain

一條以 IPFS 同步與保存區塊、支援 EVM Osaka 的輕量公開 PoS 鏈。
驗證者節點的目標硬體是樹莓派級機器：4 核 ARM、4 GB RAM、128 GB SSD。

- 開發方案：<https://claude.ai/code/artifact/cd491a00-a0a2-4e92-92f3-205aa77d55c5>
- 設計決策紀錄：[`docs/adr/`](docs/adr/)
- 里程碑進度：[`docs/milestones.md`](docs/milestones.md)

## 目前狀態：M3 共識

| 項目 | 狀態 |
| --- | --- |
| 鏈參數、genesis 格式與驗證（M0） | 完成 |
| 執行、儲存、增量 MPT、出塊與匯入、交易池、JSON-RPC（M1） | 完成 |
| 區塊的 IPLD 編碼：header CID 即 block hash、1 MiB chunk、dag-cbor envelope、CAR | 完成：`crates/ipld` |
| libp2p：QUIC/TCP、Kademlia、gossipsub 公告、Bitswap 1.2.0、交易轉發 | 完成：`crates/net` |
| 跟隨節點：收公告 → Bitswap 抓資料 → 重新執行；落後時沿 parent 回填 | 完成：`crates/sync`、`boltchain follow` |
| 與標準 IPFS 互通（Helia 以 Bitswap 取回區塊並驗證） | 已驗證：`scripts/interop/` |
| 主網 genesis 範本（驗證者、多簽、bootstrap 網域） | 完成，待驗證者填入 BLS 公鑰 |
| BLS12-381 驗證者金鑰、持有證明、與收款地址的綁定（`boltchain keys`） | 完成 |
| 共識：兩鏈 HotStuff + Jolteon 規則、QC/TC、最終性證明（ADR 0005） | 完成：`crates/consensus` |
| 決定性模擬器：10,000 個故障情境無安全違規 | 完成：`crates/sim`、`bolt-sim` |
| 驗證者節點：投票前取得並重新執行區塊；7 個節點關掉 2 個仍持續出塊 | 完成：`boltchain validator` |
| 委員會與質押合約、增發（M4） | 下一步 |
| Osaka state tests、ARM 基準 | 在 CI 上執行 |

## 快速開始

```sh
cargo test --workspace
cargo run --release -p boltchain -- devnet          # 出塊者；JSON-RPC 在 http://127.0.0.1:8545
cargo run --release -p boltchain -- follow --producer <peer id> \
  --bootnode /ip4/127.0.0.1/udp/8017/quic-v1/p2p/<peer id> --p2p-port 8018   # 跟隨者；RPC 在 8546
cargo run --release -p bolt-sim -- --scenarios 10000   # 共識模擬器
cargo run --release -p boltchain -- bench           # 區塊執行基準
cargo run -p boltchain -- genesis inspect genesis/devnet.json
scripts/statetest.sh                                 # 需要能連到 github.com
```

devnet 的使用方式（MetaMask、Foundry）見 [`docs/devnet.md`](docs/devnet.md)。

### 本地跑 7 個驗證者（dev 鏈）

```sh
for i in 0 1 2 3 4 5 6; do
  cargo run -q --release -p boltchain -- keys dev --index $i --out data/v$i/key.json
done
cargo run -q --release -p boltchain -- node-id --node-key data/v0/node.key   # 第一個節點的 peer id
# 第一個節點
cargo run --release -p boltchain -- validator --key data/v0/key.json --datadir data/v0 \
  --p2p-port 9000 --rpc 127.0.0.1:8545 --block-time 1
# 其餘節點（i = 1..6），以第一個節點的位址為 bootnode
cargo run --release -p boltchain -- validator --key data/v$i/key.json --datadir data/v$i \
  --p2p-port 900$i --rpc 127.0.0.1:854$((5+i)) --block-time 1 \
  --bootnode /ip4/127.0.0.1/udp/9000/quic-v1/p2p/<v0 的 peer id>
```

### 主網驗證者金鑰

```sh
boltchain keys new --out validator-key.json                 # 權限 0600；請離線備份
boltchain keys binding-message --key validator-key.json --address <收款地址>
# 用收款地址的錢包以 personal_sign 簽署上面印出的文字
boltchain keys genesis-entry --key validator-key.json --name <名稱> \
  --fee-recipient <收款地址> --binding-signature <簽章>
```

把印出的 JSON 交給 genesis 維護者，填入 `genesis/mainnet.template.json`。

## Workspace 結構

```
crates/
  primitives/  鏈參數、genesis 格式
  exec/        revm Osaka 執行、區塊執行器
  store/       libmdbx 儲存層、路徑式增量 MPT、狀態歷史
  chain/       genesis 初始化、出塊與匯入
  txpool/      交易池
  rpc/         eth_* JSON-RPC
  node/        `boltchain` 執行檔：validator、follow、devnet、keys、bench、genesis 工具
  ipld/        IPLD 編碼、CAR
  net/         libp2p、gossipsub、Bitswap 1.2.0、交易轉發
  sync/        公告、跟隨、回填
  consensus/   兩鏈 HotStuff（Jolteon 規則）、QC/TC、BLS 聚合、最終性證明
  sim/         決定性共識模擬器（bolt-sim）
contracts/     系統合約（Foundry，M4）
genesis/       genesis 檔（devnet.json：主網格式測試用；dev.json：本地開發鏈；mainnet.template.json：主網範本）
scripts/       statetest、與 Helia 的互通測試
```

## 授權

MIT，見 [`LICENSE`](LICENSE)。
