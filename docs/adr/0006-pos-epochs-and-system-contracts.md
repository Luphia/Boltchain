# ADR 0006：M4 PoS——epoch 交接、委員會、系統合約、獎勵與罰沒

- 狀態：已採納（2026-09-25）
- 相關：開發方案「共識」「經濟模型與系統合約」兩節、ADR 0002（不加客製預編譯）、ADR 0005（共識規則）

這份 ADR 把方案裡還沒定的細節定下來，實作以此為規格。

## 1. epoch 與高度

- epoch 以**區塊高度**劃分：epoch `e` 包含高度 `e·L+1` 到 `(e+1)·L`，`L = epoch_slots`（主網 14,400）。genesis 屬於 epoch 0。
- 記 `start(e) = e·L+1`、`end(e) = (e+1)·L`。
- 有逾時的時候，一個 epoch 會比 24 小時稍長。這不影響安全，只影響獎勵的時間單位。

## 2. 委員會提前一個 epoch 決定並寫進鏈上狀態

- 執行 `start(e)` 這個區塊時，節點在交易之前做「epoch 前置步驟」：
  1. 讀取 `StakingManager.snapshot()`，取得所有 Active 且質押 ≥ 64 BOLT 的驗證者。
  2. 抽出 **epoch e+1** 的委員會，用 system call 寫進 `ConsensusRegistry`。
- **抽籤**：
  - 種子是 parent（也就是 `end(e-1)`）的 `mix_hash`。
  - 驗證者依 id 排序後算累積權重。第 i 席取 `keccak(seed ‖ e+1 ‖ i) mod 總質押`，再用二分搜尋找到對應的驗證者。
  - 共 `committee_size` 席，可重複抽中。成本是 `O(N + S·log N)`，只在節點端執行，不需要 Fenwick tree 合約。
- **委員會格式**：
  - `members`：不重複的驗證者 id，按第一次抽中的順序。
  - `weights`：每人的席次數。
  - `seats`：每一席的 member 索引，依抽籤順序排列，leader 就在 `seats` 上輪流。
  - 共識層的 `ValidatorSet` 就是這個結構：quorum 依席次權重計算，QC 的 bitmap 以 member 為單位。
- **啟動期**：只要還沒達到退出條件（≥ 128 個質押者且總質押 ≥ 1,000 萬 BOLT，dev 鏈可以調低），委員會就是 genesis 的啟動期驗證者，每人 1 席、等權重。
  達到條件的那個 epoch 抽出的委員會開始依質押權重抽選，`bootstrapEnded` 在同一次 system call 中設定。
- epoch 0 和 epoch 1 的委員會都來自 genesis：epoch 1 在高度 1 的前置步驟中寫入，那時還沒有質押者。
- 委員會存在狀態裡，所以任何節點在高度 ≥ `start(e-1)` 時都能直接讀出 epoch e 的委員會，不需要歷史狀態。

## 3. epoch 交接：nil 收尾區塊

問題：兩鏈規則要提交 `end(e)`，需要它的子區塊在下一輪拿到 QC。
如果子區塊由新委員會簽署，新成員沒有舊委員會的鎖定狀態，交接點就可能分叉。

決定：

- 委員會 e 不為高度 > `end(e)` 的**真實區塊**投票。
- 當 high QC 已經落在 `end(e)`（記為 X）或 X 的 nil 後代上，leader 只能提議 **nil 區塊**：
  `hash = keccak("boltchain/nil/v1" ‖ epoch ‖ parent ‖ round)`，高度是 parent + 1。nil 區塊沒有內容，不上鏈，也不需要執行。
  它只用來讓委員會 e 以正常的兩鏈規則提交 X。
- X 一旦被提交（透過 X 或它的某個 nil 後代的兩鏈），epoch e 結束。
  epoch e+1 一律從 **X** 開始，這是由高度決定的，所有誠實節點都會得到同一個 X。
  nil 區塊不影響帳本，所以「從哪個 nil 提交的」無關緊要。
- epoch e+1 的起點憑證是 **anchor QC**：`round 0, block X, height end(e)`，沒有簽章。
  它的正當性來自 X 的提交證明 `EpochProof = CommitProof(X)`，這份證明放在 `start(e+1)` 區塊 envelope 的 `qc` 欄位；header 裡的 `parent_beacon_block_root = keccak(qc)` 對它做了承諾。
- 所有簽署訊息的 domain 加上 epoch，因為每個 epoch 的 round 都從 1 重新開始：
  - `vote_msg(chain, epoch, round, block)`
  - `proposal_msg(...)`
  - `timeout_msg(...)`
- 安全性論證：
  - 委員會 e 已提交的鏈，最高只到 X；nil 區塊不在帳本裡。
  - 委員會 e+1 的所有區塊都從 X 延伸。
  - 在 epoch 內部，仍然是 ADR 0005 的 Jolteon 規則。
- 模擬器新增多 epoch 情境：每個 epoch 換一組隨機的委員會，並在交接時段注入延遲、崩潰和拜占庭行為，檢查已提交鏈互為前綴。

## 4. 最終性證明與跟隨節點

- `CommitProof` 擴充成可以證明「B 已提交」，其中 B 可能是 X，而子區塊可能是 nil：
  `{epoch, qc, child_qc, child: Header(rlp) | Nil{rounds}}`。
  `rounds` 是從 X 沿 nil 鏈到子區塊的每一輪，用來重建 nil hash。
- 跟隨節點依高度找出 epoch，從**本地狀態**讀取該 epoch 的委員會來驗證。
- 如果落後超過一個 epoch：
  1. 先沿 parent 取得所有 envelope（這部分不可信）；
  2. 從舊到新匯入；
  3. 每跨過一個 epoch 邊界，先用已知的委員會驗證 `start(e+1)` envelope 裡的 EpochProof，再繼續匯入；
  4. 最後一段在驗證 tip 的證明之前不匯入。

## 5. 一個節點多把驗證者金鑰

- 共識引擎的 `me` 從單一 index 改成一組本地 signer。
  同一個節點上的多把金鑰共用同一份安全狀態：同樣的輸入會做出同樣的決定，所以等同多個誠實節點。
- 每把在委員會裡的金鑰各自投票和送出逾時；輪到其中任何一把擔任 leader 時就提議。
- 用途：
  - 實際營運，和以太坊 validator client 一樣；
  - 在 2 vCPU 的機器上，讓 100 個驗證者的驗收測試跑得動。

## 6. RANDAO

- `extra_data = round (8 bytes) ‖ reveal (96 bytes)`。
  `reveal` 是 proposer 以 BLS 對 `("boltchain/randao/v1", chain, height)` 的簽章。BLS 簽章是唯一的，所以 proposer 只能選擇發或不發，最多造成 1 bit 的偏差。
- `mix_hash(h) = keccak(mix_hash(h-1) ‖ keccak(reveal))`，並作為 EVM 的 `prevrandao`。
  驗證者投票前會用 proposer 的公鑰檢查 reveal，所以 QC 同時也為 reveal 背書。
- nil 區塊沒有 reveal。

## 7. 系統合約

| 位址 | 合約 | 說明 |
| --- | --- | --- |
| `0xB017…0001` | StakingManager（proxy） | 註冊（公鑰 + 鏈上驗證 PoP）、追加質押、申請退出、14 epoch 後提領、改收款地址、`snapshot()`；只接受 ConsensusRegistry 的 `slash`、RewardDistributor 的 `lockReward` |
| `0xB017…0002` | ConsensusRegistry（proxy） | 目前 epoch、各 epoch 委員會、`bootstrapEnded`；接收雙重簽名證據，在鏈上驗證後罰沒 |
| `0xB017…0003` | RewardDistributor（proxy） | 流通量記帳、燃燒、每個區塊的投票紀錄、epoch 結算、提領獎勵 |
| `0xB017…0004` | ParamRegistry（proxy） | `gasLimit`（15M–60M）、`minBaseFee`、`committeeSize`（不得低於 genesis 設定的下限，主網 512） |
| `0xB017…0005` | HistoryRegistry（proxy） | M5 才實作，先部署空的實作以保留位址 |
| `0xB017…00A0` | 治理 Safe（SafeProxy → Safe 1.4.1 singleton） | 5-of-9 |
| `0xB017…00A1` | UpgradeTimelock（OpenZeppelin TimelockController） | 16 天，負責系統合約升級與安全相關參數 |
| `0xB017…00A2` | ParamTimelock | 2 天，只能在範圍內調整參數 |

- 系統合約是 UUPS proxy（OpenZeppelin `ERC1967Proxy` + `UUPSUpgradeable`），只有 UpgradeTimelock 能升級。各合約之間的位址全部寫成常數。
- **genesis 部署方式**：
  - 節點在 genesis 時直接在上述位址「執行 initcode」。`address(this)` 就是目標位址，所以 constructor 寫的 storage 和 immutable 都正確。
  - 接著以 system 地址執行初始化呼叫：Safe 的 `setup(owners, 5, …)`、登記啟動期驗證者。
  - 合約 bytecode 隨節點程式一起發布（`crates/contracts` 內嵌編譯產物），genesis.json 不需要放 bytecode。
- **工具鏈**：solc 0.8.30（npm 的 solcjs），`evmVersion = prague`（Osaka 相容）。依賴以 npm 固定版本：OpenZeppelin 5.6.1、Safe 1.4.1-2。
  `contracts/build.mjs` 產生 `contracts/out/*.json` 並提交進 repo，CI 會重新編譯並比對。
  雲端環境連不到 GitHub，所以不用 Foundry，合約測試用 Rust + revm 撰寫。
- **Safe 部署方式**：沒有使用官方的 SafeProxyFactory，而是直接在固定位址放 SafeProxy。
  singleton 的 bytecode 來自 npm 套件的編譯產物，主網前要和以太坊上 Safe 1.4.1 的 code hash 核對（列入 M7 稽核）。

## 8. system call（不計入區塊 gas）

每個區塊在交易之前，EIP-4788 與 EIP-2935 之後，依序執行：

1. `RewardDistributor.onBlock(parentBurned, certEpoch, bitmap)`
   - `parentBurned = parent.base_fee × parent.gas_used`，從流通量扣除。
   - bitmap 取自本區塊 envelope 裡的 QC（epoch 起點區塊則取 EpochProof 內的 QC），替該 epoch 的委員會成員記票。
     計數器以 16 bit 打包，每個區塊最多寫 32 個 slot。
2. 若本區塊是 `start(e)` 且 `e ≥ 1`：
   1. `payout = RewardDistributor.settle(e-1, emission)`，其中 `emission = (上限 − 流通量) × k × 80%`：
      - 依 `票數 × 席次` 分給 epoch e-1 的委員；
      - 啟動期驗證者只拿 25%，而且直接鎖進質押，啟動期結束才能動；
      - 合約回傳實際要發放的總額。
   2. 節點把 `payout` 直接加到 RewardDistributor 的餘額，流通量增加同樣的數字。沒發出去的部分留在未發行池。
   3. 存儲獎勵的 20% 要到 M5 才有抽查機制，M4 先不鑄造。
3. 若本區塊是 `start(e)`：抽 epoch e+1 的委員會，執行 `ConsensusRegistry.beginEpoch(e, committee(e+1), bootstrapEnded)`。

在 genesis 起算的整條鏈上，所有帳戶餘額加總應該等於「流通量 + 被罰沒而鎖死的金額」；測試會逐塊檢查這個不變量。

## 9. 罰沒

- **證據**：
  - 同一個 epoch、同一輪的兩張不同投票，或兩個不同的提案。
  - 提交者附上驗證者的**非壓縮**公鑰和兩個非壓縮簽章（EIP-2537 格式）。合約先確認壓縮後與登記的公鑰相符，再以 EIP-2537 做 hash-to-curve 和 pairing 驗證。
    hash-to-curve 用 SHA-256 預編譯做 `expand_message_xmd`，用 MODEXP 對 p 取模，再用 `MAP_FP2_TO_G2`。
  - 證據只在 14 個 epoch 內有效。
- **處罰**：
  - `罰沒比例 = min(100%, 5% + 3 × 最近 14 個 epoch 被罰總額 / 總質押)`，並強制退出。
  - 1% 給舉報者，其餘鎖死並從流通量扣除。
  - 被罰的啟動期驗證者從啟動期委員會中移除。
- **偵測**：驗證者節點收到同一個 signer 在同一輪的兩張衝突投票時，先驗證兩個簽章，然後把可以直接送出的 `submitEvidence` calldata 寫到 `consensus/evidence/<epoch>-<round>-<id>.json`，並記錄 error log。
  自動送交易需要一個付 gas 的帳戶，這部分移到 M6。
- **gas**：`register`（含鏈上 PoP 驗證）428,634 gas，`submitEvidence`（兩次 BLS 驗證）572,075 gas。

## 10. 不在 M4 範圍

- 投票直送 leader：100 個驗證者時，gossip 的流量約 20 KB/輪，還不是瓶頸，移到 M6 與 sentry 節點一起處理。
- 存儲獎勵與抽查（M5）。
- ParamRegistry 的 `gasLimit`、`minBaseFee` 在 M4 就會讀取：每個區塊從 parent 狀態讀取，所以修改從下一個區塊生效。它們本身受 2 天 timelock 保護。`committeeSize` 在 epoch 起點讀取。

## 11. 啟動期驗證者的鎖倉獎勵：抽籤權重設上限（2026-09-25 決定）

**問題**：依方案，啟動期驗證者拿 25% 的投票獎勵，自動質押並鎖倉。若鎖倉獎勵完全算作質押：

- 7 位啟動期驗證者每天共鎖入約 40.8 萬 BOLT，約 25 天就能自己達成「總質押 ≥ 1,000 萬」的退出條件；
- 退出後他們握有大部分質押，委員會實際上由他們主導。

M4 驗收測試中，外部驗證者各押約 100 BOLT 時，抽出的委員會全部是啟動期驗證者。

**決定**：鎖倉獎勵在抽籤時設上限。

- 抽籤權重 = 未鎖倉質押 + min(鎖倉獎勵, `lockedWeightCap`)。`StakingManager.snapshot()` 回傳的就是這個權重。
- 退出條件的「總質押」也用同樣的權重計算，所以啟動期驗證者無法靠鎖倉獎勵自己達標。
- `lockedWeightCap` 預設為「退出門檻的平均質押」= 1,000 萬 ÷ 128 = **78,125 BOLT**。每位啟動期驗證者的鎖倉獎勵最多只跟一位平均質押者一樣重。dev 鏈依其退出門檻計算。
- 這個上限存在 ParamRegistry，只有 16 天 timelock 能改，而且不得低於 64 BOLT。
- 上限只影響抽籤權重，不影響資金：罰沒比例仍以實際質押計算，提領也是全額。

**已知限制**：上限跟著「仍在質押中的鎖倉部分」。啟動期結束後，啟動期驗證者可以退出、等 15 個 epoch 的解除質押期、提領後重新註冊，這樣上限就不再適用。這個過程有成本（期間沒有獎勵），而且公開可見。如果要進一步限制，需要另外的機制（例如按地址追蹤），目前不做。

**效果**：同一個驗收情境（外部驗證者各約 100 BOLT、每位啟動期驗證者鎖倉約 5.8 萬 BOLT）下，epoch 2–6 共 80 席中，啟動期驗證者只佔 2 席。

## 12. 驗證結果

- 合約單元測試（Rust + revm）：
  - Solidity 的 BLS 驗證與 blst 結果一致（壓縮、簽章、PoP、錯誤訊息與錯誤公鑰都會被拒絕）；
  - genesis 部署（Safe 5-of-9、兩個 timelock 的角色與延遲）；
  - 註冊、退出與 14 epoch 解除質押、暫停存入；
  - 雙重投票罰沒（5%、1% 舉報獎勵、流通量扣除、重放拒絕、相關性加重）；
  - 升級必須經過 Safe → 16 天 timelock，參數有硬性上下限。
- 鏈層測試（每個 epoch 4 個區塊，單一出塊者）：逐塊檢查「帳戶餘額加總 = 流通量 + 罰沒鎖死額」，以及：
  - 啟動期委員會；
  - 質押者達標後改依質押抽籤；
  - 啟動期 25% 鎖倉；
  - 啟動期後的獎勵可以提領，而且與席次成正比。
- 模擬器：約一半的情境每 3–12 個區塊換一次委員會（隨機子集、隨機席次）。
  - seeds 0–9,999：99,437 次 epoch 交接，0 安全違規、0 活性失敗。
  - seeds 10,000–19,999：97,746 次 epoch 交接，0 安全違規、0 活性失敗。
  - 第一版產生器沒有限制拜占庭節點在單一委員會的席次比例：seed 19148 的某個委員會讓拜占庭節點拿到 43% 的席次，因而出現安全違規。這超出 BFT 的前提，不是協定錯誤。產生器已改為保證每個委員會的拜占庭席次低於 1/3。上面兩批是修正後的結果。
- 端對端（`crates/node/tests/m4_pos.rs`）：
  - 100 個驗證者在 epoch 0 以交易註冊，由 5 個節點共同運行 107 把金鑰；
  - epoch 2–6 的委員會各不相同，共有數十位驗證者輪流上任；啟動期驗證者因鎖倉權重上限只佔 80 席中的 2 席；
  - 注入的雙重投票被節點偵測並寫出證據，證據以交易送出後，該驗證者被罰沒 5%；
  - 新的跟隨節點從 genesis 開始，逐個 epoch 驗證證明，同步到 epoch 6；
  - release 約 43 秒，debug 約 46 秒。
