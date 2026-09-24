# ADR 0004：自行實作 Bitswap 1.2.0

- 狀態：已採納（2026-09-25）

## 背景

方案原本打算用 beetswap（rust-libp2p 上的 Bitswap 實作）。但 beetswap 0.5 依賴 `multihash-codetable 0.1`，
後者依賴的 `core2` 已在 crates.io 上**全部版本都被 yank**，新專案無法解析相依。beetswap 也綁定較舊的 libp2p（0.56），
而且它預設的雜湊表不含 keccak-256，要另外處理 `eth-block` CID。

## 決定

在 `bolt-net` 內用 `libp2p-stream` 自行實作 Bitswap 1.2.0（約 400 行）：

- 協定 `/ipfs/bitswap/1.2.0`，unsigned-varint 長度前綴的 protobuf，欄位編號與官方 `message.proto` 相同（以 prost derive 手寫，不需 protoc）
- 支援 want-block、want-have、HAVE / DONT_HAVE、帶 CID prefix 的 payload
- 收到的區塊一律依 CID 重新雜湊驗證，只接受 Boltchain 使用的組合（eth-block/keccak-256、raw 與 dag-cbor/sha2-256）
- 只提供本地 blockstore 裡的區塊，而 blockstore 只存放驗證過的鏈上資料

## 驗證

- 單元測試涵蓋 CID prefix 與 protobuf 欄位編碼
- 以 Helia 7.1.15（@helia/bitswap 4.0.19）實測：Helia 經 Bitswap 取回 envelope 與 header，
  以 keccak-256 驗證 header CID，digest 與 block hash 相符（`scripts/interop/`）

## 後果

- 少一個外部相依，libp2p 可以跟上最新版（0.57）
- 未實作 Bitswap 的 session、ledger 與 pending bytes 等最佳化；Boltchain 的抓取模式是「一次要一個區塊的少數 CID」，目前不需要
- 公開 IPFS 橋接（M5）需要時再補上 want-list 更新與取消的完整處理
