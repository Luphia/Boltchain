# ADR 0015：瀏覽器的存證與託管證據驗證

- 狀態：已採納並實作（2026-09-28，issue #1）
- 相關：ADR 0010（區塊鏈瀏覽器）

## 背景

有些應用把資料放在鏈下，鏈上只放壓縮證據（雜湊、Merkle root、總額），資產由合約託管，例如 CO2Exchange 的帳本合約。用戶手上有自己那一份證據，但要確認它和鏈上一致，得自己打 RPC、解 event、算雜湊。瀏覽器應該提供一個不依賴應用方網站的第三方驗證頁面。

## 決定

1. **`/verify` 頁面**。有兩種用法：
   - 上傳應用方提供的證明檔（`docs/evidence-proof-format.md`），必要時一併上傳原始檔案；
   - 只上傳檔案，指定交易（可以自動在交易的事件中尋找這個雜湊）或合約 view 函式。
2. **檔案不離開瀏覽器**。keccak-256 與 sha-256 用純 JS 實作，在 Worker 中分段計算（以 IP 位址提供的 http 頁面不能使用 WebCrypto）。報告只放雜湊。
3. **不相信節點**。
   - 事件：瀏覽器取回整個區塊的原始資料（`/api/raw/block/<n>`），核對 `keccak256(header)` 等於區塊雜湊，並重建交易與 receipt 的 trie，比對 `transactionsRoot` 與 `receiptsRoot`。
   - 託管餘額：用新的 `eth_getProof` 對 `stateRoot` 證明代幣合約的 storage slot。
   - 無法證明的部分（`eth_call` 的回傳、最終性）標示為「節點回報」。
4. **不處理歷史狀態**。節點只保留最近 128 塊的狀態歷史，`--archive` 保留的是區塊與 receipts，不是狀態；state trie 也只保留最新的節點。因此：
   - 事件證據任何時間都能驗證；
   - `eth_call` 錨點與託管餘額以查詢當下的最新狀態為準，報告中會標明查詢的區塊；
   - `eth_getProof` 只支援最新區塊，指定其他區塊時回傳錯誤。
   - 託管的「該期當下持有」可以由應用方在事件中直接記錄（合約執行當下算出的值），就不需要歷史狀態。CO2Exchange 的 `commit()` 已經在提交時檢查 `totalCash ≤ balanceOf(this)`。
5. **雜湊規則由證明檔宣告**。瀏覽器支援一份固定清單，不猜應用的樹：`prefixed-abi-v1`、`prefixed-packed-v1`、`sum-packed-v1`、`oz-standard-v1`，前三種和 CO2Exchange 的 `LedgerMerkle`、`MerkleSumTree` 逐位元組相同。
6. **報告**。
   - JSON 報告附上比對用到的原始資料，任何人都能對任一節點重做。
   - 可列印的 HTML 可以另存為 PDF，所以瀏覽器仍然沒有外部套件。
   - 報告雜湊是鍵排序後 JSON 的 keccak-256。

## 實作

- `crates/node/src/explorer/verify.js`：雜湊、RLP、MPT、ABI 編解碼、Merkle 規則與驗證流程。同一個檔案同時是頁面的函式庫、雜湊用的 Worker，以及 Node 測試的對象。
- 新 API（皆以最新狀態為準，除了 raw block）：
  - `/api/raw/block/<n>`：標頭 RLP、交易與 receipt 的 EIP-2718 編碼；
  - `/api/proof/<address>?slots=…`：EIP-1186 proof 與最新標頭；
  - `/api/call?to=…&data=…`：唯讀呼叫與它所在的區塊。

  全部同源，Explorer 放到 HTTPS 反向代理後面也不會遇到混合內容。
- RPC 新增 `eth_getProof`（只支援最新狀態）。
- `boltchain devnet --explorer <addr>`：開發鏈也能開瀏覽器。

## 驗證

- `verify_js.mjs`：以 viem、node:crypto、@ethereumjs/trie、@openzeppelin/merkle-tree 與 CO2Exchange 自己的樹產生的參考向量核對（357 項）。
- Rust：state trie proof 以 alloy-trie 的 `verify_proof` 核對。
- 端到端：在本機開發鏈上部署 CO2Exchange 的 `Ledger` 與 `MockTWD`，以 CO2Exchange 的程式產生證明檔，並以 Chromium 操作頁面，涵蓋符合、竄改、錯鏈、錯事件、缺檔、手機寬度與 40 MiB 檔案（2.6 秒）。

## 之後

- 巢狀證明：例如 `assetsRoot` 那棵小樹的葉子，要先證明它在餘額葉子裡。
- 若有需求再做完整狀態歷史與歷史 `eth_getProof`，那需要改儲存層。
- 多節點比對：同時向多個節點取區塊雜湊。
