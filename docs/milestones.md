# 里程碑

| 里程碑 | 內容 | 驗收標準 | 狀態 |
| --- | --- | --- | --- |
| M0 骨架 | workspace、CI、genesis 格式、資料庫選型 | Osaka statetest 全數通過；CI 在 ARM64 上跑測試 | 完成（CI run 36045139504：statetest、x86 與 ARM64 全數通過） |
| M1 單節點鏈 | revm 執行、MPT、MDBX、RPC，暫用單一簽名者出塊 | Foundry 部署合約、MetaMask 轉帳；30M gas / 6 s；樹莓派執行最壞情況區塊 < 2 s | 程式完成，CI（x86 與 ARM64）通過；樹莓派實測與實機 MetaMask/Foundry 待驗證 |
| M2 IPFS 同步 | IPLD 格式、內嵌 bitswap、gossipsub 公告 | 5 個跟隨節點只靠 IPFS 同步；公告到資料取完 p95 < 1 s | 程式完成，驗收測試通過（本機） |
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

- [x] Osaka state tests 在 CI 上全數通過（GitHub Actions run 36045139504）
- [x] ARM64 上的測試與基準跑完（數據在該次 CI 的 `block execution benchmark` 步驟日誌）
- [ ] 樹莓派實機測試
- [ ] 以真正的 MetaMask 與 Foundry 手動驗證一次（自動化測試已涵蓋同樣的 RPC 流程）

## M2 結果（2026-09-25）

完成項目：

- [x] `ipld`：header 用 `eth-block`（0x90）+ keccak-256，CID 的 digest 就是 block hash；body 切成 ≤ 1 MiB 的 raw chunk；dag-cbor envelope 以 `parent` 串起整條鏈；CAR v1 讀寫
- [x] `store`：`ipld` 與 `envelopes` 資料表；區塊的 IPLD 資料與狀態在同一個寫入交易提交
- [x] `net`：QUIC/TCP + DNS（支援 `/dnsaddr`）、Kademlia（`/bolt/<chain id>/kad/1.0.0`）、gossipsub 公告（只接受指定出塊者簽署的訊息）、交易轉發
- [x] Bitswap 1.2.0：自行實作，與 Kubo/Helia 線上格式相容（見 ADR 0004）；以 Helia 7.1.15 實測可取回並驗證區塊
- [x] `sync`：收到公告 → 驗證 envelope 與 header → 用 Bitswap 抓 chunk（先向轉發者要，對方回 DONT_HAVE 或 150 ms 內沒回應就改問其他節點）→ 重新執行並核對 header 與 envelope；落後時沿 `parent` 回填
- [x] `boltchain follow`、`boltchain node-id`；出塊者每塊發公告並接收轉發的交易
- [x] 主網 genesis 範本：7 個驗證者收款地址、9 位多簽簽署者（EIP-55 校驗碼全數正確）、bootstrap `node001`–`node009.cafeca.io`

### 驗收測試（`crates/node/tests/m2_sync.rs`）

1 個出塊者 + 5 個跟隨者，其中 3 個跟隨者只連到另一個跟隨者、不直接連出塊者。24 個區塊輪流包含：空塊、小 body（內嵌在公告裡）、約 190 KB body（走 Bitswap）、約 1.4 MB body（兩個 chunk）。
全部跟隨者的 head 與 envelope root 都與出塊者一致；第 6 個晚加入的節點只連到其中一個跟隨者，沿 parent 回填 25 個區塊。

| 建置 | 公告 → 資料到手 p50 | p95 | 最大值 |
| --- | --- | --- | --- |
| release | 3 ms | 58 ms | 164 ms |
| debug（CI 跑的版本，7 個節點共用 2 vCPU） | 92 ms | 661 ms | 1,145 ms |

這是同一台機器上的 localhost 數字。跨網路的實際延遲要到 M6 測試網才量得到；每多一跳大約增加一個 RTT 加上一次 1.4 MB 的傳輸時間。

### 尚未完成

- [ ] 驗證者 BLS 公鑰（M3 的 `boltchain keys`）
- [ ] 公開 IPFS 網路的橋接與 epoch 索引（M5）
- [ ] NAT 穿透（AutoNAT、Circuit Relay v2、DCUtR）與連線數上限（M6）

## M3 結果（2026-09-25）

完成項目：

- [x] `boltchain keys`：用 BLS12-381（blst，min_pk，以太坊 DST）產生金鑰和持有證明（PoP）。`feeRecipient` 地址以 EIP-191 personal_sign 簽署綁定聲明。`genesis-entry` 會先驗證 PoP 和綁定，再印出 genesis 條目
- [x] `consensus`：兩鏈 HotStuff 加上 Jolteon 的投票與逾時規則（見 ADR 0005）。狀態機是純函式，含 QC/TC、BLS 聚合簽章、最終性證明 `CommitProof`、安全狀態的持久化與恢復
- [x] `sim`：決定性模擬器，涵蓋延遲、丟包、GST、崩潰重啟和 5 種拜占庭行為（包括串通的連續惡意 leader）。CI 每次跑 10,000 個情境
- [x] `chain`：尚未最終的區塊透過 overlay 疊在已提交狀態上推測執行。同一個 parent 可以有多個候選，提交後其餘的自動丟棄
- [x] DA 投票規則：投票前必須以 Bitswap 取得區塊的全部 chunk、寫入 blockstore、在 parent 上重新執行，且 header 相符
- [x] `boltchain validator`：共識、出塊、提交，並在提交後發出帶最終性證明的公告。跟隨節點只匯入帶有效證明的區塊
- [x] `net`：新增共識 gossipsub topic `/bolt/<chain id>/consensus`

### 驗收

| 項目 | 結果 |
| --- | --- |
| 模擬 10,000 個故障情境，無安全違規 | 情境 0–9,999 與 10,000–19,999：0 安全違規、0 活性失敗（release 各約 65 秒） |
| 故意改壞程式，確認模擬器抓得到錯 | 移除鎖定規則：3,000 個情境中抓到 65 次違規。同輪兩票：沒抓到（見 ADR 0005） |
| 關掉 2 個節點仍持續出塊（`crates/node/tests/m3_consensus.rs`） | 7 個驗證者到高度 5 後關掉 2 個，其餘 5 個再推進 5 塊以上且 head 一致；只信任最終性證明的跟隨節點同步到同一條鏈。連跑 4 次都通過，每次約 30 秒 |

### 尚未完成（移到後續里程碑）

- [ ] leader 依質押權重做 VRF 抽樣，委員會由系統合約決定（M4）
- [ ] 投票直送下一任 leader，不走 gossip 廣播（M4/M6）
- [ ] 模擬器加入能精準分割誠實節點的對手，用來抓「同輪兩票」這類變異（M4）
- [ ] 加密 keystore（EIP-2335）（M6）
- [ ] 主網 genesis 填入 7 位驗證者的 BLS 公鑰、PoP 和綁定簽章（由各驗證者用 `boltchain keys` 產生）

## M4 待辦（下一步）

- [ ] 系統合約（Foundry）：質押（最低 64 BOLT）、解除質押等待期、罰沒、委員會快照
- [ ] 每個 epoch 用 BLS-VRF 依權重抽樣委員會（目標 512 席），在 epoch 邊界交接
- [ ] 增發：每個 epoch 依 `epoch_emission` 發放；EIP-1559 base fee 銷毀；總量上限 2^32 BOLT
- [ ] 多簽 + timelock 治理合約（9 位簽署者）
- [ ] 雙重投票與雙重提案的證據提交與罰沒
- [ ] 驗收：多 epoch devnet 上委員會輪替、質押變動生效、罰沒正確
