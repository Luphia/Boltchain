# 里程碑

| 里程碑 | 內容 | 驗收標準 | 狀態 |
| --- | --- | --- | --- |
| M0 骨架 | workspace、CI、genesis 格式、資料庫選型 | Osaka statetest 全數通過；CI 在 ARM64 上跑測試 | 完成（CI run 36045139504：statetest、x86 與 ARM64 全數通過） |
| M1 單節點鏈 | revm 執行、MPT、MDBX、RPC，暫用單一簽名者出塊 | Foundry 部署合約、MetaMask 轉帳；30M gas / 6 s；樹莓派執行最壞情況區塊 < 2 s | 程式完成，CI（x86 與 ARM64）通過；樹莓派實測與實機 MetaMask/Foundry 待驗證 |
| M2 IPFS 同步 | IPLD 格式、內嵌 bitswap、gossipsub 公告 | 5 個跟隨節點只靠 IPFS 同步；公告到資料取完 p95 < 1 s | 程式完成，驗收測試通過（本機） |
| M3 BFT 共識 | HotStuff-2、模擬器、DA 投票規則，固定 7 人驗證者集 | 10,000 個故障情境無安全違規；關掉 2 個節點仍持續出塊 | 完成 |
| M4 PoS | 系統合約、BLS-VRF、512 席抽籤、獎勵、罰沒、啟動期 | 100 個驗證者完成 epoch 輪替；鏈上驗證雙重簽名證據 | 完成（啟動期設計由 M4.5 取代） |
| M4.5 PoW 啟動 | RandomBOLT 挖礦、ASERT、分叉選擇與重組上限、移除治理、自動切換到 PoS | 5 礦工 + 10 跟隨節點；重組上限生效；從 genesis 同步驗證 PoW；自動切換到 PoS | 完成（本機驗收） |
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

## M4 結果（2026-09-25）

設計見 ADR 0006。完成項目：

- [x] 系統合約（Solidity 0.8.30，npm 的 solcjs 編譯；產物提交在 `contracts/out`，CI 會重新編譯比對）：
  - StakingManager：註冊時在鏈上驗證 PoP，最低 64 BOLT，退出後 14 epoch 可提領，可暫停存入
  - ConsensusRegistry：各 epoch 的委員會；雙重投票與雙重提案證據在鏈上驗證，罰沒 5%–100%（相關性加重），1% 給舉報者
  - RewardDistributor：流通量記帳（燃燒、罰沒）、每區塊記票、epoch 結算
  - ParamRegistry：參數有硬性上下限
  - HistoryRegistry：先保留位址，M5 實作
  - 治理：Safe 1.4.1 5-of-9，加上兩個 OpenZeppelin TimelockController（16 天、2 天）
  - 系統合約都是 UUPS proxy，只有 16 天 timelock 能升級
- [x] Solidity BLS 函式庫：RFC 9380 hash-to-curve（SHA-256 預編譯、MODEXP、EIP-2537），與 blst 交叉驗證
- [x] genesis：在固定位址直接執行 initcode 部署系統合約，再以 system 地址初始化。devnet genesis hash 改為 `0x207c7090…8ea0c`
- [x] 每區塊的 system call：記錄燃燒與投票；epoch 起點結算上一個 epoch 的獎勵，並依質押權重抽下一個 epoch 的委員會（512 席，可重複抽中）
- [x] RANDAO：`extra_data` 帶 proposer 的 BLS reveal，投票前驗證
- [x] 共識：
  - 訊息與簽章加入 epoch；
  - epoch 交接用 nil 收尾區塊與 anchor QC；
  - 席次加權的 leader 輪值；
  - 一個引擎可以替多把本地金鑰簽署。
- [x] 模擬器：一半的情境每 3–12 個區塊換一次委員會（隨機子集、隨機席次）；節點在 epoch 結束時切換引擎，落後的節點用 epoch 證明追上
- [x] 驗證者節點：
  - 從鏈上狀態讀取委員會，一個節點可以持有多把金鑰；
  - 保存 epoch 證明；
  - 偵測到雙重投票時寫出可以直接送出的證據 calldata。
- [x] 跟隨節點：落後超過一個 epoch 時，逐個 epoch 驗證證明再匯入
- [x] 網路層：送往節點的事件不再阻塞 swarm（節點處理不及時丟棄 gossip，而不是讓 Bitswap 停擺）；追趕匯入移到獨立的 task
- [x] dev profile 對所有相依套件開最佳化（revm、blst 不最佳化時慢 10–50 倍，多節點測試會餓死）

### 驗收

| 項目 | 結果 |
| --- | --- |
| 100 個驗證者跑完 epoch 輪替（`crates/node/tests/m4_pos.rs`） | 100 個驗證者（各約 100 BOLT）在 epoch 0 以交易註冊，由 5 個節點運行 107 把金鑰；epoch 2–6 的委員會各不相同，數十位驗證者輪流上任，啟動期驗證者只佔 80 席中的 2 席；新的跟隨節點從 genesis 逐個 epoch 驗證證明，同步到 epoch 6。release 約 43 秒、debug 約 46 秒 |
| 鏈上驗證雙重簽名證據並罰沒 | 注入的雙重投票被節點偵測、寫出證據，以交易送出後，該驗證者被罰沒 5% 並失去抽籤資格。`submitEvidence` 需 572,075 gas，`register` 需 428,634 gas |
| 流通量不變量（`crates/chain/src/epoch_tests.rs`） | 逐塊檢查「餘額加總 = 流通量 + 罰沒鎖死額」，包括燃燒、增發、鎖倉、提領 |
| 共識模擬 | 見下方 |

模擬器第一次跑 seeds 10,000–19,999 時，seed 19148 出現 2 次安全違規。原因是隨機產生的委員會讓拜占庭節點拿到 43% 的席次，超出 BFT 的前提（< 1/3），不是協定錯誤。產生器已改為保證每個委員會的拜占庭席次低於 1/3，之後重跑的結果記在 ADR 0006 §12。

### 決定

- [x] 啟動期驗證者的鎖倉獎勵在抽籤與退出條件中設權重上限，預設 78,125 BOLT（ADR 0006 §11）。驗收情境中，啟動期驗證者只佔 80 席中的 2 席

### 尚未完成（移到後續里程碑）

- [ ] 存儲獎勵（發行的 20%）與歷史抽查（M5）
- [ ] 證據自動送交易（需要付 gas 的帳戶）、投票直送 leader、sentry 節點（M6）
- [ ] 交易 gossip：目前交易只進入收到它的節點的交易池（M6）

## 方向調整：PoW 啟動（2026-09-25，ADR 0007）

專案負責人決定：鏈在初期不依賴 CAFECA 的驗證者，也沒有任何特殊待遇的錢包。
啟動期改為「CPU 友善的 PoW 起步，質押最終性逐步接手」，分三個階段：

- 階段 A：純 PoW
- 階段 B：PoW 出塊，質押委員會確認檢查點
- 階段 C：PoS

M4 的 PoS 機制完整保留，用在階段 B 與階段 C；啟動期驗證者相關的程式碼會在 M4.5 移除。

### M4.5 PoW 啟動期：結果（2026-09-25）

- [x] RandomBOLT：RandomX 1.2.1 原始碼放在 `third_party/randomx`，只改 Argon2 salt（`RandomBOLT\x01`），Monero 的算力不能直接挪用。light mode 驗證（雲端 x86 每次 28 ms），fast mode 挖礦；種子每 2,048 塊更換、延遲 64 塊。每把金鑰共用一份 cache，VM 以池管理，驗證與挖礦可以並行
- [x] header 封印：`difficulty` 放難度，`nonce` 放 PoW nonce，輸入是「nonce 清零的 header hash ‖ nonce」；ASERT（aserti3-2d 三次多項式），目標 12 秒、半衰期 1 小時，主網起始難度 2¹⁸
- [x] 分叉選擇：累積難度最大者勝，同分取 hash 較小者（每個節點選得一樣）。儲存層新增累積難度表與「倒回 head」，用狀態歷史反向套用並核對 state root。自動重組上限 128 塊，也不會越過 PoS 區塊
- [x] PoW 同步：公告不帶證明；跟隨節點沿 envelope 的 parent 往回找到已知區塊（可以在別的分支上），檢查每個封印後交給分叉選擇。PoS 區塊的祖先如果是挖出來的，也走封印驗證
- [x] `boltchain mine`、`validator --mine`；每塊獎勵 = 未發行量 × 6.59×10⁻⁸ × 80%，直接記給礦工並計入流通量
- [x] 移除啟動期驗證者、鎖倉獎勵、鎖倉權重上限。genesis 不列任何驗證者；dev 鏈可以用 `devValidators` 在 genesis 質押，從第 1 塊就跑 PoS（M3/M4 的測試改用這個）
- [x] 移除治理（ADR 0008）：Safe、兩個 timelock、UUPS proxy、ParamRegistry、暫停權限都刪除；系統合約直接部署在固定地址，管理函式只接受 system 地址。`0xB017…0004` 留空
- [x] gasLimit 由出塊者投票（每塊變動小於 parent 的 1/1024，範圍 15M–60M，`--gas-target`）；委員會大小、最低 base fee 是 genesis 常數
- [x] 硬分叉表（目前是空的）與不規則狀態變更（替換合約程式碼、寫 storage）；分叉 ID（EIP-2124）放在 identify 的 agent 字串，不相容的 peer 會被斷線，看到更新的分叉會提醒升級
- [x] 階段 A → C：每個 epoch 起點檢查 T2（主網 128 人、1,000 萬 BOLT），連續 14 個 epoch 成立就排定 PoS 從兩個 epoch 後開始；終端區塊是 PoS 前一個 epoch 的最後一塊；首個委員會只算滿 14 個 epoch 的質押；之後的 PoW 區塊一律無效
- [x] 首個 PoS epoch 以終端區塊為 anchor，第一塊不帶證明（和 genesis 的下一塊一樣）。若在委員會認證任何區塊之前，較重的分支換掉了終端區塊，引擎換到新 anchor 重新開始，而且不會在用過的 round 再簽名
- [x] 模擬器（`crates/sim/src/pow.rs`，可重現）：
  - 20 個誠實礦工、1 秒傳播：孤塊率約 7%，平均出塊 12–14 秒，最深重組 1 塊
  - 起始難度估高 10 倍：一天內平均出塊仍在 24 秒以內
  - 60% 算力私下挖約 10 分鐘再公開：誠實節點跟著重組（最深 43 塊），這是階段 A 已知的風險
  - 同樣的攻擊私下挖超過 128 塊：被拒絕，誠實節點留在自己的鏈上
  - 33% 自私挖礦：拿到約 41% 的區塊。同分取小 hash 等同隨機選邊，門檻約 25%（first-seen 規則在攻擊者連線好時更差），這是階段 A 的已知代價，要靠階段 B 補
  - 網路分區 10 分鐘：重新連上後較輕的一邊重組（23 塊）；分區 1 小時：超過上限，兩邊各自維持，需要營運者介入
- [x] 轉換點奪取：在首個委員會抽籤前一刻質押 50 倍資金的大戶拿不到席位，下一個 epoch 才有（`crates/chain/src/pow_tests.rs`）

| 驗收項目 | 結果 |
| --- | --- |
| 5 個礦工節點 + 10 個跟隨節點，自動切換到 PoS（`crates/node/tests/m45_pow.rs`） | RandomBOLT（light mode）挖礦；4 位驗證者在挖礦階段以交易註冊，第 9、17 塊滿足門檻，PoS 排在 epoch 4，第 32 塊是終端區塊；之後的區塊全部由委員會產生，15 個節點停在同一條鏈上，至少兩位礦工挖到區塊。debug 約 160 秒 |
| 終端區塊之後的 PoW 區塊無法接受 | 同一測試：接在終端區塊後面的 PoW 區塊被拒絕 |
| 從 genesis 同步會驗證 PoW | 同一測試：新節點逐塊檢查封印，接著逐個 epoch 驗證 PoS 證明 |
| 51% 攻擊下重組上限生效 | `chain` 測試：比 128 塊更深的較重分支被拒絕，head 不變；模擬器同上 |

已知限制（M4.6 處理）：

- 委員會已經在某個終端區塊上認證區塊之後，如果有跟隨節點改跟了另一條更重、終端區塊不同的 PoW 分支，它驗不了 PoS 證明，會卡住。階段 B 先確認深度 ≥ 32 的檢查點，可以消除轉換點的分歧
- 重組時先倒回再匯入，期間 RPC 會短暫看到較低的 head
- 交易仍沒有 gossip（M6）；挖礦節點只打包自己交易池裡的交易

### M4.6 質押最終性（預估 2–3 週）

- [ ] 階段 B：委員會以 BFT 確認深度 ≥ 32 的 PoW 檢查點，分叉選擇不越過已確認的檢查點
- [ ] 階段 B 的獎勵分配（礦工 / 確認投票者，建議 60% / 40%）
- [ ] 完整的 A → B → C 流程與對應的最終性證明
- [ ] 驗收：多數算力攻擊被質押最終性擋下；小額質押奪取委員會的企圖被 T1 門檻擋下

### 決定

- [x] 治理：規則只能靠硬分叉修改，鏈上沒有任何治理權限；節點預留自動更新機制，預設只提醒（ADR 0008）

## M5 待辦（M4.6 之後）

- [ ] 檢查點同步：從 CAR 快照加最終性證明起步（不超過 14 天的弱主觀性檢查點）
- [ ] 狀態與區塊修剪
- [ ] 歷史分片與抓取抽查（HistoryRegistry，16 人審計小組），存儲獎勵
- [ ] 公開 IPFS 橋接：Bitswap want-list 更新與取消、epoch 索引 CID
- [ ] 驗收：新節點從快照起步追上最新區塊；從公開 gateway 讀得到歷史 CAR

## M6 追加項目（ADR 0008）

- [ ] 發布清單（m-of-n ed25519 簽章、IPFS 發布、gossipsub 公告）與驗證
- [ ] `--auto-update off|notify|apply`（預設 notify）、`--update-signers`、`--pin-version`；7 天冷卻期、分叉預告期檢查、啟動失敗自動回滾
- [ ] 可重現建置與 CI 建置證明
- [ ] 測試網演練一次硬分叉（含不規則狀態變更）
