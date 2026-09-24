# ADR 0001：本地資料庫採用 libmdbx + 自寫資料表

- 狀態：已採納（2026-09-25）

## 背景

EVM 執行主要的成本是隨機讀取狀態；寫入是每塊一次的批次寫入，而且只有一個寫入者。
驗證者硬體是樹莓派級機器，要避免背景 compaction 造成 CPU 與 I/O 尖峰。

## 選項

| 選項 | 優點 | 缺點 |
| --- | --- | --- |
| A. reth-db（git 相依） | 有現成的以太坊資料表與 reth-trie | 相依樹大、API 常變、沒有發佈到 crates.io |
| **B. libmdbx + 自寫資料表** | MDBX 的讀取效能、相依少 | 要自寫增量 state root（約 2–3 週） |
| C. redb | 純 Rust | 狀態成長到數十 GB 時缺乏前例 |
| D. RocksDB | 寫入吞吐高 | compaction 尖峰、C++ 建置、寫入放大 |

## 決定

採用 B。IPFS blockstore 放在同一個 MDBX 環境的另一張表，讓 pin 狀態與區塊執行在同一個交易內提交。

## 後果

- 增量 state root 必須有「整棵樹重算」的對照測試（property-based）。
- 依賴 C 工具鏈建置 libmdbx；交叉編譯到 ARM64 在 CI 上驗證。
