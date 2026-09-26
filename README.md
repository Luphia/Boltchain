# Boltchain

**把閒置的機器變成收入：出租算力與儲存空間，以 BOLT 結算。**

Boltchain 是一條相容 EVM（Osaka）的公開鏈，區塊與資料都存放在 IPFS 上。除了一般的智能合約，鏈上內建兩個租賃市場：

- **儲存租賃（SwarmStorage）**：付費請多個節點保存你的加密檔案，節點要持續接受抽查，保存失敗會被罰款並換人。
- **算力租賃（ComputeMarket 與 TEE Agent 租用）**：付費請別人的機器跑 AI 推論，或長時間執行一個 AI Agent；執行者看不到內容，交付結果依規則結算。

兩個市場都讓「有機器、沒有 BOLT」的人也能加入：註冊可以只簽名、由別人代送；押金從收入中逐步累積，不必先付。

- 設計決策紀錄：[`docs/adr/`](docs/adr/)
- 里程碑進度：[`docs/milestones.md`](docs/milestones.md)
- 公開測試網（chain 8018）：[`docs/testnet.md`](docs/testnet.md)，瀏覽器 <http://211.22.118.149:8080/>
- 開發方案：<https://claude.ai/code/artifact/cd491a00-a0a2-4e92-92f3-205aa77d55c5>

> 目前是測試網階段。主網尚未上線，合約與參數在外部稽核前仍可能調整。

## 儲存租賃：SwarmStorage

> 狀態：**已實作**。開發鏈從 genesis 就有；公開測試網在第 2401 塊的硬分叉 `swarm` 啟用（ADR 0014）。

**委託者**

1. 檔案在你的電腦上用 [`bolt-vault`](crates/vault) 加密：分塊加密、Reed-Solomon 容錯（預設 4 + 2）、只有你的金鑰能解開。保存者與審計者只看得到密文。
2. 你指定副本數（1–16）、保存期間與願意付的單價（BOLT / GiB / epoch），款項預付進合約託管。
3. 合約從開放報價、價格與容量符合的提供者中隨機抽出保存者。他們從你的節點取走資料，之後每個完整的 epoch 領一次款；保存結束後，剩餘款項退還給你。
4. 可以延長、提前取消，或在任何節點取回檔案。

```sh
boltchain validator ... --rpc 127.0.0.1:8545 --rpc-storage    # 你的節點接收要保存的區塊
boltchain storage put --wallet me.json report.pdf --replicas 3 --epochs 30 --price 0.5
boltchain storage deal 0                                      # 查看副本與付款狀態
boltchain storage get --wallet me.json 0 --out report.pdf
```

**提供者**

- 驗證者，以及不需質押的存儲提供者（`wallet storage-register`，沒有 BOLT 時可以只簽名、請別人代送）都能開放報價：

  ```sh
  boltchain storage offer --wallet me.json --id <提供者 id> --capacity 102400 --price 0.2
  boltchain storage settle --wallet me.json <dealId>        # 領取已完成 epoch 的款項
  boltchain storage withdraw --wallet me.json
  ```

- 節點自動取得並保存分配到的資料。
- 未質押的提供者：每筆收入先扣 20% 累積押金（上限 64 BOLT）；可承接的未付款項最多是 5 BOLT + 10 × 押金。

**如何確認資料真的還在**

- 每個 epoch 從委員會抽出審計小組，隨機抽 16 個委託（加上 16 個鏈歷史）的某份副本的某個區塊，只向該保存者要。
- 小組三分之二簽章的結果寫上鏈：
  - **通過**：照常領款。
  - **失敗**：驗證者罰 1% 質押，未質押者燒 10% 押金並停權；這份副本上次領款之後的款項不付，合約立即抽新的保存者接手。
- 同一套抽查也保護鏈本身的歷史：舊 epoch 由 16 位驗證者與 16 位未質押提供者保存，通過抽查的人平分每塊 1 BOLT 的存儲獎勵。

## 算力租賃

### AI 推論工作：ComputeMarket

> 狀態：**合約已完成**（`0xB017…0006`，每條鏈 genesis 就有）；執行者與委託者的命令列工具、驗證小組的重跑比對尚未完成（ADR 0011）。

- 委託者存入託管款，指定模型與每千個 token 的單價；輸入輸出檔用 bolt-vault 加密，只有雙方看得到。
- 執行者只需簽名接單、簽名交付收據，由任何人代送交易；收入直接入帳，**從頭到尾不必持有 BOLT**。
- 樂觀結算：委託者核可，或爭議窗口過後任何人都能結算。
- 爭議時付 1 BOLT 押金，由驗證小組裁決；執行者錯了罰押金 10%，7 天沒有裁決視為執行者勝。
- 押金從收入中扣 20% 累積到 64 BOLT；接單上限 5 BOLT + 10 × 押金。

### 長時間執行 AI Agent：TEE 租用

> 狀態：**設計已採納、尚未實作**（ADR 0013）。

租用別人機器上的可信執行環境（TEE）執行你的 AI Agent：

- 執行者看不到 Agent 的記憶、金鑰與對話，也改不了它的行為。
- Agent 的狀態每小時加密備份到 SwarmStorage 並在鏈上登記版本號，可以隨時搬到另一台機器；執行者拿舊狀態重啟會被拒絕。
- 按小時計費，依 enclave 簽名的檢查點付款；不運行就不付錢。
- TEE 被攻破的證據（同一版本簽出兩份不同狀態）會讓執行者**失去全部押金**，機器列入黑名單。

支援兩個信任等級，租約上標明：

| 等級 | 硬體 | 證明的驗證 |
| --- | --- | --- |
| T1 機密 VM | Intel TDX、AMD SEV-SNP | TDX 用 Osaka 內建的 P-256 預編譯直接在鏈上驗證；SNP 由驗證小組背書 |
| T2 量測開機裝置 | DGX Spark、Intel AI PC、AMD AI PC | 專用系統開機、TPM 量測，由驗證小組背書 |

T2 裝置沒有機密 VM，對實體攻擊的防護比 T1 弱得多；三款裝置的支援程度還要實機確認。Agent 的 LLM 呼叫外部 API，所以 API 業者看得到提示內容。

## 為什麼能做到

- **資料在 IPFS 上**：區塊、快照、使用者檔案都是以 CID 定址的 IPFS 區塊。任何 IPFS 節點都能取得並驗證，不必信任提供資料的人。
- **CPU 就能開始**：鏈從 RandomBOLT 挖礦開始（CPU 友善），不需要先有 BOLT；質押足夠之後，先由委員會確認挖出的區塊，再自動切換到 PoS（ADR 0007）。
- **公平發行**：沒有創世分配、沒有特殊錢包。每塊 32 BOLT（共識 31 + 存儲 1），每 4 年減半，最後固定為每塊 2 BOLT，沒有總量上限（ADR 0012）。
- **沒有管理員**：系統合約不可升級、沒有暫停鍵，規則只能靠硬分叉修改（ADR 0008）。
- **小機器也能跑**：節點的目標硬體是樹莓派級（4 核 ARM、4 GB RAM、128 GB SSD）；舊的歷史分散保存，新節點從快照起步。

## 目前進度

| 領域 | 狀態 |
| --- | --- |
| 執行、儲存、交易池、JSON-RPC（EVM Osaka） | 完成 |
| IPLD 區塊編碼、libp2p、Bitswap、與標準 IPFS 互通 | 完成；Kubo 可從 head 下載到 genesis |
| 共識：HotStuff（Jolteon 規則），10,000 個故障情境模擬無安全違規 | 完成 |
| RandomBOLT 挖礦 → 委員會確認 → PoS 的完整轉換 | 完成，有驗收測試 |
| 儲存層：epoch 索引、狀態快照、檢查點同步、修剪、歷史抽查、IPFS 閘道 | 完成 |
| 發行改制：固定區塊獎勵、未質押存儲提供者 | 完成 |
| **SwarmStorage：付費保存使用者資料** | **完成**；測試網第 2401 塊啟用 |
| **ComputeMarket：AI 推論市場** | **合約完成**；命令列工具與驗證小組重跑待做 |
| bolt-vault：加密分片格式（Rust 與 WASM） | 完成 |
| 內建區塊鏈瀏覽器（繁中 / 英文） | 完成 |
| 公開測試網（4 節點、8 位驗證者）、Uniswap v4 | 運行中 |
| **TEE Agent 租用** | **已採納設計，未實作** |

## 剩餘工作

依重要性排序：

1. **算力市場可以實際使用**（M7）
   - 驗證小組的裁決憑證（BLS 聚合，比照抽查）；爭議時把檔案加密給小組、小組重跑比對
   - `boltchain provider` / `boltchain job`（先支援 llama.cpp）、中繼服務、瀏覽器頁面
   - `ModelRegistry`
2. **TEE Agent 租用**（ADR 0013）
   - 第零期：DGX Spark、Intel AI PC、AMD AI PC 實機確認（TPM、量測開機、開機韌體保護、記憶體加密、IOMMU）
   - 第一期：T2 的 TPM 驗證、`TdxVerifier` 與 `TeeCollateral`、`ImageRegistry`、`EnclaveLease`、Agent 執行環境映像（x86-64 / arm64）、限權錢包
   - 第二期：保管小組（租用者離線也能自動搬家）、SEV-SNP；第三期：GPU 機密運算
3. **SwarmStorage 補強**：刪除已結束委託的資料、依金額加權抽查、提供者拒收、副本分散到不同機器
4. **網路與維運**（M6）：測試網觀察一週（含 PoS 轉換與外部節點）、Helia 停止送 want 的問題、修剪改成事件驅動、簽名發布與自動更新、可重現建置、樹莓派 7 天基準
5. **主網前**：外部稽核、參數定案（以 BOLT 計價的各種門檻與押金）、bug bounty、主網 genesis 與 bootstrap 節點；Uniswap v4 主網授權（BUSL-1.1）

## 快速開始

```sh
cargo test --workspace
cargo run --release -p boltchain -- devnet          # 單一出塊者的開發鏈；JSON-RPC 在 http://127.0.0.1:8545
cargo run --release -p boltchain -- bench           # 區塊執行基準
(cd contracts && npm ci && node build.mjs)           # 重新編譯系統合約（產物已提交在 contracts/out）
```

devnet 的使用方式（MetaMask、Foundry）見 [`docs/devnet.md`](docs/devnet.md)；加入公開測試網見 [`docs/testnet.md`](docs/testnet.md)。

### 沒有 BOLT，只有一台機器

```sh
boltchain wallet new --out me.json
# 1. 挖礦（挖礦階段）：區塊獎勵直接記到你的地址
boltchain mine --genesis genesis/testnet.json --datadir data --beneficiary <地址>
# 2. 成為存儲提供者：只簽名，交給任何人代送
boltchain wallet storage-register --wallet me.json --node-key data/node.key --signed
boltchain validator --genesis genesis/testnet.json --datadir data --storage-account <地址>
# 3. 有了 BOLT 之後：質押成為驗證者（至少 64 BOLT），或開放儲存報價
boltchain wallet stake --wallet me.json --key validator-key.json --amount 64
boltchain storage offer --wallet me.json --id <提供者 id> --capacity 102400 --price 0.2
```

### 本地挖礦鏈（從 PoW 切換到 PoS）

```sh
cargo run --release -p boltchain -- mine --genesis genesis/pow-dev.json --datadir data/m0 \
  --beneficiary 0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266
```

`pow-dev.json` 有 2 位驗證者共質押 128 BOLT 並維持 2 個 epoch（64 塊）就進入階段 B，3 位共 192 BOLT 再維持 2 個 epoch 就切換到 PoS。驗證者用 `validator --genesis genesis/pow-dev.json --key <金鑰> [--mine --beneficiary <地址>]` 參與。主網礦工可加 `--randomx-fast`（每把金鑰 2 GiB 記憶體，雜湊快約 8 倍）與 `--mining-threads`。

### 成為驗證者

```sh
boltchain keys new --out validator-key.json                 # 權限 0600；請離線備份
boltchain wallet stake --wallet me.json --key validator-key.json --amount 64
boltchain wallet set-peer --wallet me.json --key validator-key.json --node-key data/node.key
```

驗證者同時負責確認區塊、擔任抽查與爭議的驗證小組，並保存分到的鏈歷史；沒有公布提供資料的節點，抽查一律算失敗（每次罰 1% 質押）。

### 節點選項

```sh
boltchain validator --checkpoint <number>:<hash>      # 從快照起步，不必從 genesis 重放
boltchain validator --archive                         # 保留全部區塊
boltchain validator --gateway 0.0.0.0:8080 --explorer --archive   # IPFS 閘道與區塊鏈瀏覽器
boltchain validator --rpc 127.0.0.1:8545 --rpc-storage             # 接收要保存的檔案（RPC 請綁 localhost）
boltchain history export --datadir data --epoch 3 --out epoch-3.car
```

## Workspace 結構

```
crates/
  primitives/  鏈參數、genesis 格式、硬分叉表
  exec/        revm Osaka 執行、區塊執行器
  store/       libmdbx 儲存層、增量 MPT、狀態歷史、快照與修剪
  chain/       genesis 初始化、出塊與匯入、PoW 分叉選擇與重組
  txpool/      交易池
  rpc/         eth_* 與 bolt_* JSON-RPC
  node/        `boltchain` 執行檔：mine、validator、follow、devnet、wallet、storage、keys、bench、history；儲存服務、閘道、瀏覽器
  pow/         RandomBOLT（RandomX 變體）、header 封印、ASERT
  ipld/        IPLD 編碼、CAR、epoch 索引、快照與委託索引格式
  net/         libp2p、gossipsub、Bitswap 1.2.0、交易轉發、NAT 穿透
  sync/        公告、跟隨、回填
  consensus/   兩鏈 HotStuff（Jolteon 規則）、QC/TC、BLS 聚合、最終性證明
  system/      系統合約的編譯產物、genesis 部署、ABI、epoch hook、委員會抽籤、抽查任務
  vault/       bolt-vault 加密分片格式（Rust 與 WASM；js/file_operator.ts 是瀏覽器端的上傳下載）
  sim/         決定性模擬器
contracts/     系統合約：StakingManager、ConsensusRegistry、RewardDistributor、SwarmStorage、ComputeMarket
genesis/       genesis 檔（見 genesis/README.md）
scripts/       statetest、IPFS 互通測試、Uniswap v4 部署
third_party/   RandomX 原始碼（只改 Argon2 salt，見 randomx/BOLTCHAIN.md）
```

## 授權

MIT，見 [`LICENSE`](LICENSE)。
