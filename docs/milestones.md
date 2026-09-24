# 里程碑

| 里程碑 | 內容 | 驗收標準 | 狀態 |
| --- | --- | --- | --- |
| M0 骨架 | workspace、CI、genesis 格式、資料庫選型 | Osaka statetest 全數通過；CI 在 ARM64 上跑測試 | 程式完成，等 CI 首次執行 |
| M1 單節點鏈 | revm 執行、MPT、MDBX、RPC，暫用單一簽名者出塊 | Foundry 部署合約、MetaMask 轉帳；30M gas / 6 s；樹莓派執行最壞情況區塊 < 2 s | 程式完成；ARM 數據等 CI，實機 MetaMask/Foundry 待手動驗證 |
| M2 IPFS 同步 | IPLD 格式、內嵌 bitswap、gossipsub 公告 | 5 個跟隨節點只靠 IPFS 同步；公告到資料取完 p95 < 1 s | 未開始 |
| M3 BFT 共識 | HotStuff-2、模擬器、DA 投票規則，固定 7 人驗證者集 | 10,000 個故障情境無安全違規；關掉 2 個節點仍持續出塊 | 未開始 |
| M4 PoS | 系統合約、BLS-VRF、512 席抽籤、獎勵、罰沒、啟動期 | 100 個驗證者完成 epoch 輪替；鏈上驗證雙重簽名證據 | 未開始 |
| M5 儲存層 | 檢查點同步、CAR 快照、修剪、歷史分片與抽查 | 新節點從快照起步追上最新區塊 | 未開始 |
| M6 強化與測試網 | peer scoring、NAT 穿透、防罰沒、樹莓派基準 | 樹莓派驗證者 7 天參與率 > 99%，RSS < 1.5 GB | 未開始 |

## M1 結果（2026-09-25）

完成項目：

- [x] `store`：libmdbx 資料表、路徑式增量 MPT（對照 alloy-trie 全量重算的 property test，另跑 5,000 組隨機案例）、最近 128 塊的狀態歷史與修剪
- [x] `exec`：區塊執行器（EIP-4788 / EIP-2935 system call、收據與 bloom、EIP-1559 base fee、EIP-7934 區塊大小、拒收 blob）
- [x] `chain`：出塊與匯入。匯入端會重新執行並逐欄比對 header；不符就中止寫入交易，什麼都不落地
- [x] `txpool`：admission 規則（EIP-155、chain id、拒收 type 3、gas 範圍、手續費下限、nonce、餘額、每帳戶 16 筆、10% 替換加價）
- [x] `rpc`：25 個 `eth_*` / `net_*` / `web3_*` 方法，含 CORS
- [x] `node`：`boltchain devnet`、`boltchain bench`
- [x] 端到端測試：alloy 客戶端經 HTTP JSON-RPC 轉帳、部署合約、呼叫合約、查 log、查歷史餘額（`crates/node/tests/e2e.rs`）

### 基準測試（雲端 x86_64，2 vCPU；ARM 數據由 CI 產生）

| 情境 | 每塊 gas | 出塊（執行 + 提交） | 驗證者匯入（驗簽 + 重新執行 + 提交） |
| --- | --- | --- | --- |
| 1,428 筆轉帳 | 30.0M | 15 ms | 50 ms |
| BN254 pairing 塞滿 | 28.6M | 340 ms | 330 ms（最壞 332 ms） |
| BLS12-381 pairing 塞滿 | 29.4M | 259 ms | 254 ms（最壞 260 ms） |

過程中做的兩項調整：

- BN254 後端從 substrate-bn 改回 revm 預設的 arkworks：最壞情況區塊從 754 ms 降到 332 ms。
- 匯入時的簽章還原改為多核平行，並改用 libsecp256k1：轉帳區塊的匯入時間從 309 ms 降到 50 ms。

樹莓派的數字還沒有實測。若照「樹莓派比這台慢 3–5 倍」粗估，最壞情況約 1.0–1.7 秒，在 2 秒目標內，但餘裕不大。
GitHub 的 ARM64 runner 比樹莓派快，只能當下限參考；最終要在實機上跑 `boltchain bench`。

### 尚未完成

- [ ] Osaka state tests 實際執行（等 CI）
- [ ] ARM64 基準數據（等 CI），以及樹莓派實機測試
- [ ] 以真正的 MetaMask 與 Foundry 手動驗證一次（自動化測試已涵蓋同樣的 RPC 流程）

## M2 待辦（下一步）

- [ ] `ipld`：eth-block (0x90) header、1 MiB body chunk、dag-cbor envelope、epoch index、CAR
- [ ] `store`：blockstore 資料表與 pin 狀態
- [ ] `net`：libp2p（QUIC、Kademlia、gossipsub、bitswap / beetswap、request-response）
- [ ] 出塊者公告 → 跟隨節點用 bitswap 抓取 → `Chain::import_block`
- [ ] 驗收：5 個跟隨節點只靠 IPFS 同步；從公告到取完資料 p95 < 1 s
