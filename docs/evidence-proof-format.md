# 存證證明檔格式 v1

區塊鏈瀏覽器的 `/verify` 頁面（ADR 0015）讀這個格式。應用方替每個用戶產出一份 JSON 證明檔，用戶拿著它與原始檔案，就能在任何 Boltchain 瀏覽器上自行驗證，不需要應用方的網站還在。

檔案只在瀏覽器裡計算雜湊，不會上傳。驗證結果可以下載成 JSON 報告或列印成 PDF。

## 範例：Merkle Sum Tree 的持有證明（CO2Exchange 帳本）

```json
{
  "version": 1,
  "chainId": 8018,
  "anchor": {
    "type": "log",
    "contract": "0x…Ledger",
    "txHash": "0x…",
    "logIndex": 0,
    "event": "Committed(uint64 indexed epoch, bytes32 anchor, (bytes32 prev, uint64 epoch, bytes32 logRoot, bytes32 balanceRoot, bytes32 registryRoot, bytes32 identityRoot, uint256 totalKg, uint256 totalCash, bytes32 totalsHash, uint64 upToBlock, uint64 lastSeq, uint16 rulesVersion) commitment)",
    "field": "commitment.balanceRoot",
    "sums": ["commitment.totalKg", "commitment.totalCash"]
  },
  "scheme": "sum-packed-v1",
  "leaf": {
    "types": ["address", "uint64", "bytes32", "uint256", "uint256"],
    "values": ["0x7099…79C8", "1", "0x21e2…c663", "1750", "650000000"],
    "names": ["account", "epoch", "assetsRoot", "kg", "cash"],
    "sums": [3, 4]
  },
  "siblings": [{ "hash": "0x…", "sums": ["80", "380000005"] }],
  "path": "0x0",
  "custody": { "holder": "0x…Ledger", "token": "0x…TWDC", "sum": 1 }
}
```

## 欄位

### 最上層

| 欄位 | 必填 | 說明 |
|---|---|---|
| `version` | 建議 | 固定為 `1` |
| `chainId` | 建議 | 和瀏覽器所在的鏈不同時，驗證結果為「不符合」 |
| `anchor` | ✅ | 鏈上證據在哪裡，見下方 |
| `scheme` | | 雜湊規則。不填時直接比對：葉子的雜湊必須等於 `anchor.field` |
| `leaf` | ✅ | 葉子內容 |
| `siblings`、`path` | 有 `scheme` 時 | Merkle 路徑 |
| `custody` | | 託管資產，見下方 |

### `anchor`：事件（`"type": "log"`）

| 欄位 | 說明 |
|---|---|
| `txHash` | 發出事件的交易 |
| `logIndex` | 事件在**區塊內**的索引（和 `eth_getLogs` 的 `logIndex` 相同）。有 `event` 時可以省略，取這筆交易中第一個符合簽章（與 `contract`）的事件 |
| `contract` | 發出事件的合約；填了就會檢查 |
| `event` | 帶參數名稱的事件簽章，例如 `Transfer(address indexed from, address indexed to, uint256 value)`。topic0 由簽章算出並核對。tuple 寫成 `(…)` 或 `tuple(…)` |
| `field` | 要取的欄位，用名稱或位置、以 `.` 分隔：`commitment.balanceRoot`、`2.3` |
| `sums` | Merkle Sum Tree 用：root 的每一個總額對應的欄位 |

沒有 `event` 時，事件以原始的 32 bytes 欄位讀取：`topics.1`、`topics.2`…（`topics.0` 是事件簽章），以及 `data.0`、`data.1`…。

驗證方式：瀏覽器取回整個區塊的原始資料，核對 `keccak256(header)` 等於區塊雜湊，並以 Merkle-Patricia trie 重算交易與 receipt，必須等於標頭中的 `transactionsRoot` 與 `receiptsRoot`。事件因此不必相信節點。

### `anchor`：合約 view 函式（`"type": "call"`）

| 欄位 | 說明 |
|---|---|
| `to` | 合約地址 |
| `function` | 函式簽章與回傳型別，例如 `commitmentOf(uint64 e) view returns ((bytes32 logRoot, bytes32 balanceRoot, …) c)`；也可以另外用 `returns` 欄位寫回傳型別 |
| `args` | 參數陣列 |
| `field` | 回傳值的欄位，例如 `c.balanceRoot` |

呼叫一律以**查詢當下的最新狀態**執行（節點只保留最近 128 塊的狀態歷史），回傳值由節點提供，無法用區塊雜湊證明。所以只適合值寫入後就不會改變的情況（例如依期數存放的 root），報告中也會標示「節點回報」。可以用事件時，優先用事件。

### `leaf`

以下擇一：

- `{"file": "keccak256"}` 或 `{"file": "sha256"}`：用上傳檔案的雜湊。
- `{"contentHash": "0x…"}`：直接給 32 bytes 的內容雜湊。
- `{"types": […], "values": […]}`：欄位型別與數值。

數值一律可以寫成字串（大整數請務必用字串）；`bytesN` 與 `address` 用 `0x` 十六進位。

選填的 `names` 是欄位名稱，只用於顯示；`sums` 見 `sum-packed-v1`。

### `scheme`

| 名稱 | 葉子 | 節點 |
|---|---|---|
| （不填） | `contentHash`、檔案雜湊，或 `keccak256(abi.encode(values))`，直接和 `field` 比對 | — |
| `prefixed-abi-v1` | `keccak256(abi.encode(bytes1(0x00), contentHash))`；`contentHash` 是上面的其中一種 | `keccak256(abi.encode(bytes1(0x01), left, right))` |
| `prefixed-packed-v1` | `keccak256(0x00 ‖ abi.encodePacked(values))`（給 `contentHash` 或檔案時，就是那 32 bytes） | `keccak256(0x01 ‖ left ‖ right)` |
| `sum-packed-v1` | `keccak256(0x00 ‖ abi.encodePacked(values))`；`leaf.sums` 列出哪幾個值（必須是 `uint256`）是總額 | `keccak256(0x01 ‖ L.hash ‖ L.sums… ‖ R.hash ‖ R.sums…)`，每個總額為 32 bytes；父節點總額 = 左 + 右，不得為負或溢位 |
| `oz-standard-v1` | OpenZeppelin `StandardMerkleTree`：`keccak256(keccak256(abi.encode(values)))` | 兩個子節點排序後 `keccak256(a ‖ b)` |

- 對應 CO2Exchange：`LedgerMerkle` 是 `prefixed-abi-v1`，`MerkleSumTree.leaf/parent` 是 `sum-packed-v1`，`assetLeaf` 那棵小樹是 `prefixed-packed-v1`。
- `siblings`：前三種是從葉子往上的兄弟節點；`sum-packed-v1` 每個兄弟是 `{"hash", "sums"}`；其餘是 32 bytes 的雜湊。
- `path`：位元圖（數字、十進位字串或 `0x` 字串），第 i 位為 1 代表第 i 個兄弟在**左邊**；或寫成 `[0, 1, …]` 陣列，意思相同。落單往上帶、沒有兄弟的那一層不佔位元。`oz-standard-v1` 不需要 `path`。
- 驗證時重算 root，必須等於 `anchor.field`；有總額時，root 的每個總額也必須等於 `anchor.sums` 指的欄位。

### `custody`（選填）

| 欄位 | 說明 |
|---|---|
| `holder` | 託管合約 |
| `token` | 託管的 ERC-20 |
| `sum` | 和 root 的第幾個總額比較（從 0 起算） |
| `slot` | 選填：代幣 `balances` mapping 的 storage slot。不填時自動在 slot 0–14 與 OpenZeppelin upgradeable ERC20 的 ERC-7201 位置中尋找 |

瀏覽器以 `balanceOf(holder)` 查詢，再以 `eth_getProof` 對最新區塊的 `stateRoot` 證明該 storage slot 的值。結果是**目前**持有，不是證據所在區塊的持有，因此只列為參考，不影響「符合／不符合」。

## 報告

JSON 報告包含：

- 證明檔本身；
- 上傳檔案的大小與雜湊（不含內容）；
- 用到的鏈上原始資料：區塊號、區塊雜湊、標頭 RLP、交易與 receipt 的 EIP-2718 編碼、log、`eth_call` 的參數與回傳、storage proof；
- 每一項檢查的結果，以及資料的可信程度（對區塊雜湊驗證、本機計算、節點回報、最新狀態）；
- `reportHash`：去掉 `reportHash` 欄位、鍵排序後的 JSON 的 keccak-256。

任何人都能拿這些資料，向任一個 Boltchain 節點以 `eth_getBlockByNumber`、`eth_getTransactionReceipt`、`eth_call`、`eth_getProof` 重做一次。
