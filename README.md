# Boltchain

一條以 IPFS 同步與保存區塊、支援 EVM Osaka 的輕量公開鏈。
鏈從 CPU 友善的挖礦（RandomBOLT）開始，任何人第一天就能參與；質押足夠之後自動切換到 PoS（ADR 0007）。
沒有創世分配、沒有特殊待遇的錢包、鏈上沒有治理權限，規則只能靠硬分叉修改（ADR 0008）。
節點的目標硬體是樹莓派級機器：4 核 ARM、4 GB RAM、128 GB SSD。

- 開發方案：<https://claude.ai/code/artifact/cd491a00-a0a2-4e92-92f3-205aa77d55c5>
- 設計決策紀錄：[`docs/adr/`](docs/adr/)
- 里程碑進度：[`docs/milestones.md`](docs/milestones.md)
- 公開測試網（chain 8018）：[`docs/testnet.md`](docs/testnet.md)

## 目前狀態：M5 儲存層

| 項目 | 狀態 |
| --- | --- |
| 鏈參數、genesis 格式與驗證（M0） | 完成 |
| 執行、儲存、增量 MPT、出塊與匯入、交易池、JSON-RPC（M1） | 完成 |
| 區塊的 IPLD 編碼：header CID 即 block hash、1 MiB chunk、dag-cbor envelope、CAR | 完成：`crates/ipld` |
| libp2p：QUIC/TCP、Kademlia、gossipsub 公告、Bitswap 1.2.0、交易轉發 | 完成：`crates/net` |
| 跟隨節點：收公告 → Bitswap 抓資料 → 重新執行；落後時沿 parent 回填 | 完成：`crates/sync`、`boltchain follow` |
| 與標準 IPFS 互通（Helia 以 Bitswap 取回區塊並驗證） | 已驗證：`scripts/interop/` |
| 主網 genesis 範本（無驗證者、無多簽、bootstrap 網域） | 完成，只差啟動時間 |
| BLS12-381 驗證者金鑰、持有證明、註冊交易（`boltchain keys`） | 完成 |
| 共識：兩鏈 HotStuff + Jolteon 規則、QC/TC、最終性證明（ADR 0005） | 完成：`crates/consensus` |
| 決定性模擬器：10,000 個故障情境無安全違規 | 完成：`crates/sim`、`bolt-sim` |
| 驗證者節點：投票前取得並重新執行區塊；7 個節點關掉 2 個仍持續出塊 | 完成：`boltchain validator` |
| 系統合約：質押、委員會、獎勵；不可升級、沒有治理權限（ADR 0006、0008） | 完成：`contracts/`、`crates/system` |
| 每個 epoch 依質押抽委員會、nil 收尾交接、RANDAO、增發與燃燒記帳 | 完成 |
| 雙重簽名證據在鏈上驗證並罰沒 | 完成 |
| 100 個驗證者的 epoch 輪替、跟隨節點跨 epoch 同步 | 已驗收：`crates/node/tests/m4_pos.rs` |
| RandomBOLT 挖礦、ASERT、分叉選擇、重組上限 128 塊、PoW 同步（M4.5） | 已驗收：`crates/node/tests/m45_pow.rs` |
| 質押達門檻後自動切換到 PoS；首個委員會只抽成熟質押 | 已驗收 |
| gasLimit 由出塊者投票；硬分叉表與分叉 ID | 完成 |
| 階段 B：質押委員會以 BFT 確認 PoW 檢查點、60/40 獎勵、完整 A → B → C（M4.6） | 已驗收：`crates/node/tests/m46_finality.rs` |
| 儲存層：epoch 索引、狀態快照、檢查點同步、修剪、歷史分片與抽查、公開閘道（M5，ADR 0009） | 已驗收：`crates/node/tests/m5_storage.rs` |
| Osaka state tests、ARM 基準 | 在 CI 上執行 |

## 快速開始

```sh
cargo test --workspace
cargo run --release -p boltchain -- devnet          # 出塊者；JSON-RPC 在 http://127.0.0.1:8545
cargo run --release -p boltchain -- follow --producer <peer id> \
  --bootnode /ip4/127.0.0.1/udp/8017/quic-v1/p2p/<peer id> --p2p-port 8018   # 跟隨者；RPC 在 8546
cargo run --release -p bolt-sim -- --scenarios 10000   # 共識模擬器（挖礦階段的情境在 cargo test -p bolt-sim）
cargo run --release -p boltchain -- bench           # 區塊執行基準
cargo run -p boltchain -- genesis inspect genesis/devnet.json
scripts/statetest.sh                                 # 需要能連到 github.com
(cd contracts && npm ci && node build.mjs)           # 重新編譯系統合約（產物已提交在 contracts/out）
```

devnet 的使用方式（MetaMask、Foundry）見 [`docs/devnet.md`](docs/devnet.md)。

### 本地挖礦鏈（從 PoW 切換到 PoS）

```sh
cargo run --release -p boltchain -- mine --genesis genesis/pow-dev.json --datadir data/m0 \
  --beneficiary 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266
```

`pow-dev.json` 有 2 位驗證者共質押 128 BOLT 並維持 2 個 epoch（64 塊）就進入階段 B（委員會確認挖出來的區塊），
3 位共 192 BOLT 再維持 2 個 epoch 就切換到 PoS。
驗證者用 `validator --genesis genesis/pow-dev.json --key <金鑰> [--mine --beneficiary <地址>]` 參與，
註冊交易由 `keys register-tx` 產生。主網礦工可加 `--randomx-fast`（每把金鑰 2 GiB 記憶體，雜湊快約 8 倍）與 `--mining-threads`。

### 本地跑 7 個驗證者（dev 鏈，genesis 質押，從第 1 塊就是 PoS）

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

一個節點可以重複 `--key` 同時運行多位驗證者（例如 `--key a.json --key b.json`），它們共用同一份共識安全狀態。

### 成為驗證者

```sh
boltchain keys new --out validator-key.json                 # 權限 0600；請離線備份
boltchain keys register-tx --key validator-key.json --fee-recipient <收款地址>
```

把印出的 `to` 與 `data` 從持有質押的錢包送出，金額至少 64 BOLT。所有人規則相同，genesis 不列任何驗證者。

驗證者也是歷史資料的保存者（ADR 0009）：用 owner 錢包公布提供資料的節點，否則抽查一律算失敗、每次罰 1% 質押。

```sh
boltchain node-id --node-key data/validator/node.key       # 節點的 peer id
boltchain keys set-peer-tx --id <驗證者 id> --node-key data/validator/node.key
```

### 儲存層選項

```sh
# 從快照起步，不必從 genesis 重放（需要最近一個 epoch 結尾區塊的雜湊，來源要可信）
boltchain validator --datadir data/new --checkpoint <number>:<hash>
# 保留全部區塊（預設只留最近 7 個 epoch 與分到的舊 epoch）
boltchain validator --archive
# 以 HTTP 提供 trustless IPFS gateway：/ipfs/<cid>?format=car、/history/epoch/<n>.car
boltchain validator --gateway 127.0.0.1:8080
# 離線匯出一個 epoch 的 CAR（可直接 ipfs dag import）
boltchain history export --datadir data/validator --epoch 3 --out epoch-3.car
```

## Workspace 結構

```
crates/
  primitives/  鏈參數、genesis 格式
  exec/        revm Osaka 執行、區塊執行器
  store/       libmdbx 儲存層、路徑式增量 MPT、狀態歷史、狀態快照與修剪
  chain/       genesis 初始化、出塊與匯入、PoW 分叉選擇與重組
  txpool/      交易池
  rpc/         eth_* JSON-RPC
  node/        `boltchain` 執行檔：mine、validator、follow、devnet、keys、bench、history、genesis 工具；儲存服務與閘道
  pow/         RandomBOLT（RandomX 變體）、header 封印、ASERT、每塊獎勵
  ipld/        IPLD 編碼、CAR、epoch 索引與快照格式
  net/         libp2p、gossipsub、Bitswap 1.2.0、交易轉發
  sync/        公告、跟隨、回填
  consensus/   兩鏈 HotStuff（Jolteon 規則）、epoch 交接、QC/TC、BLS 聚合、最終性證明
  system/      系統合約：編譯產物、genesis 部署、ABI、epoch hook、委員會抽籤
  sim/         決定性模擬器：共識引擎（bolt-sim）與挖礦階段
contracts/     系統合約（Solidity 0.8.30，solcjs 編譯，沒有外部相依）
genesis/       genesis 檔（見 genesis/README.md）
third_party/   RandomX 原始碼（只改 Argon2 salt，見 BOLTCHAIN.md）
scripts/       statetest、與 Helia 的互通測試
```

## 授權

MIT，見 [`LICENSE`](LICENSE)。
