# ADR 0009：儲存層：epoch 索引、狀態快照、檢查點同步、修剪、歷史分片與抽查

- 狀態：已採納並實作（2026-09-25，M5）

> **2026-09-26：`HistoryRegistry` 由 `SwarmStorage` 取代（地址與歷史規則不變），抽查同時涵蓋付費的使用者資料，見 ADR 0014。**
>
> **2026-09-26：§5 的存儲獎勵改為每塊 1 BOLT（不再是發行的 20%），並開放給未質押的存儲提供者（每個舊 epoch 另外多 16 份副本），見 ADR 0012。**
- 決定者：專案負責人
- 相關：ADR 0004（自建 Bitswap）、ADR 0006（系統合約）、ADR 0007（PoW 啟動與質押最終性）、ADR 0008（沒有治理）

## 背景

每個節點都保存從 genesis 起的全部區塊與狀態，這在樹莓派上撐不久；新節點從 genesis 重放也越來越慢。
IPFS 讓區塊天然可以被任何人取得與驗證，但必須有人真的保存舊資料，而且要能證明他們有保存。

## 決定

### 1. epoch 索引記錄在鏈上

- 每個 epoch 的第一塊，節點把上一個 epoch 所有區塊的 envelope CID 依序建成 dag-cbor 索引：
  - 根節點 `{v, epoch, first, count, groups}`
  - 每個 group 最多 1024 個 envelope CID
- 由系統呼叫 `HistoryRegistry.recordEpoch(epoch, cid)` 寫入。每個節點各自建出同一個 CID，狀態根保證大家一致。
- 從鏈上狀態就能找到全部歷史：索引 → envelope → header 與 body 分塊。

### 2. 狀態快照（不上鏈）

- 每個 epoch 的最後一塊之後，節點把完整的平面狀態依地址順序切成確定性的分塊：
  - 帳戶分塊約 512 KiB；大 storage 可以跨塊延續（`x` 旗標）
  - 合約程式碼分塊依 code hash 排序
  - 根節點 `SnapshotRoot {number, hash, state_root, td, envelope, accounts, codes}`
- 同一個區塊由誠實節點產生的快照 CID 完全相同。
- 節點保留最近 2 份；新快照完成後，舊快照只刪除新快照沒有共用的分塊。
- 快照對應的區塊成為最終（PoS 區塊、階段 B 檢查點，或被埋超過 128 塊）之後，節點在 `/bolt/<chain>/storage` 公告 `{number, hash, root, at}`。
  - 每 5 秒重播一次。`at` 讓每次重播都是新訊息，因為 gossipsub 會丟掉近期重複的訊息，否則晚加入的節點聽不到。
- 快照不寫上鏈：驗證靠使用者信任的檢查點雜湊，不需要共識。

### 3. 檢查點同步（弱主觀性）

`--checkpoint <number>:<hash>`（可選 `--snapshot <cid>`）只能用在空的資料目錄。

1. 等到對應區塊的快照公告（或直接用給定的 CID）。
2. 從 Bitswap 取回根節點、全部分塊、該區塊的 envelope、header 與 body，以及往前的祖先 envelope 與 header。
   - 往前取到 `min(256 塊, 本 epoch 起點)`：EVM 的 `BLOCKHASH` 需要前 256 塊，下一塊記錄本 epoch 的索引也需要。
3. 在同一個寫入交易中：
   - 清空狀態，套用分塊，重建 MPT。
   - 算出的狀態根必須等於 header 的 `state_root`，而 header 的雜湊必須等於信任的雜湊；祖先逐一核對 parent hash。
4. 該區塊成為 head、最終檢查點（fork choice 不會越過）與本地歷史的起點（`base`）。之後的區塊照常同步：
   - PoS 靠每個 epoch 的最終性證明，這一段的委員會已經在快照狀態裡
   - PoW 靠封印驗證

信任模型與以太坊的 checkpoint sync 相同：營運者從自己信任的來源取得最近 epoch 結尾區塊的雜湊，例如另一個自己的節點、區塊瀏覽器或朋友。
快照中的 `td`（累積難度）沒有被 header 驗證；但檢查點之後的所有分支都從同一個最終區塊出發，`td` 的偏移不影響它們之間的比較。

### 4. 修剪與歷史分片

- **保留窗口**：最近 `history_recent_epochs`（主網 7）個 epoch 的完整區塊人人都留。
- **更舊的 epoch**：以 rendezvous hashing 指派給 16 位驗證者，也就是 `keccak(索引 CID ‖ 驗證者 id)` 最小的 16 位（ADR 0006 的 `HISTORY_REPLICATION`）。
  - 驗證者集合改變時，只有離開者的份額換人。
- **節點的行為**：
  - 被指派的 epoch：保留，缺什麼就從網路補齊（索引 → envelope → 分塊）。
  - 其他舊 epoch：刪除 body、sender、receipt、交易索引與 body 分塊；header 與 envelope 永遠保留。
- **例外**：`--archive` 關閉修剪。從快照起步的節點沒有 `base` 之前的 body。
- **RPC**：已修剪、而且有交易的區塊查不到（回傳 null），不會回傳空的 body。

### 5. 抽查與存儲獎勵

- **抽籤**：每個 epoch 的第一塊，以 parent 的 `mix_hash` 為種子抽出（系統呼叫 `beginAudits`）：
  - **審計小組**：本 epoch 委員會中最多 16 位不重複的成員
  - **16 個任務**：每個任務是（舊 epoch、該 epoch 的一位被指派者、其中一個區塊）
- **提供者的義務**：每位驗證者的 owner 用 `HistoryRegistry.setPeer(id, peerId)` 公布提供資料的節點。
  - 產生交易內容：`boltchain keys set-peer-tx`
  - 沒有公布的提供者一律抽查失敗
- **審計者的做法**：
  - 對每個任務，只向提供者公布的 peer 以 Bitswap 要抽中區塊的第一個 body 分塊；空區塊則要它的 envelope。
  - 刻意不看本機資料：`probe` 只接受來自那個 peer 的回應。
  - 在限時（預設 10 秒）內拿到並通過雜湊驗證就投「通過」，否則投「失敗」。
  - BLS 簽章的 `AuditVote` 在 storage topic 上傳播。
- **認證**：小組超過 2/3 同意同一結果時，任何節點都能聚合成 `AuditCert`。出塊者把它放進 envelope 的 `audits` 欄位。
  - 區塊執行前，節點逐一驗證：本 epoch、任務仍待定、簽署者超過 2/3、聚合簽章正確。
  - 收到的區塊只要有一張無效就整塊拒絕；出塊者自己則直接丟掉無效的。
  - 驗證通過後，系統呼叫 `recordAudit` 記錄結果。
- **結果**：
  - **通過**：每個 epoch 結束時，存儲獎勵（每塊 1 BOLT × epoch 塊數，ADR 0012）由該 epoch 的通過次數平分（`settleStorage`）。沒有人通過時這部分不發行。
  - **失敗**：罰 1% 質押（`StakingManager.penalize`，只有 HistoryRegistry 能呼叫），但不強制退出。
- **儲存提供者就是質押的驗證者**：
  - 不另設儲存角色，罰則直接作用在質押上。
  - 委員會本身就在線，擔任審計者沒有額外的協調成本。

### 6. 公開閘道

- `--gateway <addr>` 以 HTTP 提供 IPFS trustless gateway 的子集：
  - `GET /ipfs/<cid>?format=raw|car`，也接受 `Accept: application/vnd.ipld.raw` 或 `application/vnd.ipld.car`
  - `GET /history/epoch/<n>.car`：從鏈上的索引直接找到 epoch
- CAR 走訪會跟隨所有 dag-cbor 連結，但不跟隨 envelope 的 `parent`，所以一個 epoch 的索引剛好匯出這個 epoch。
- 內容全部以 CID 自我驗證，使用者不需要信任閘道。
- 離線匯出：`boltchain history export --epoch <n>`，產生的檔案可以直接 `ipfs dag import`。

## 後果

- 樹莓派節點只需要保存：最近 7 個 epoch、自己被指派的舊 epoch、全部 header 與 envelope（每塊約 200 B）、目前狀態。
- 新節點的起步時間取決於狀態大小，與鏈長無關。
- 抽查只證明提供者在被問的當下能給出抽中的分塊，不是完整的可檢索性證明。
  - 提供者可以臨時向別人取得再轉交；但這要求網路上仍有其他副本，而那正是我們要的結果。
- 空區塊的抽查目標是 envelope，而 envelope 人人都留，所以這類任務較弱。

## 已知限制與後續

- 快照建立期間持有一個讀取交易。狀態很大時，資料庫檔案會在這段時間內成長。
- 修剪巡檢每 2 秒掃過所有舊 epoch；主網規模需要改成游標與事件驅動（M6）。
- 檢查點比節點保留的最舊快照（約 2 個 epoch）更舊時無法直接起步。之後可以改成：接受較新的快照，並從它回溯 header 驗證到檢查點。
- 審計投票與快照公告沒有 peer scoring 或速率限制（M6）。
- 公開閘道沒有速率限制，建議放在反向代理之後。
