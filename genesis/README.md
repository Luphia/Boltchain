# Genesis 檔

`devnet.json` 是開發用的 genesis。其中的 BLS 公鑰與多簽地址都是佔位值，
**不是有效的 BLS12-381 曲線點**；M3 加入 blst 之後，genesis 驗證會檢查曲線點與持有證明（proof of possession）。

```sh
cargo run -p boltchain -- genesis inspect genesis/devnet.json
```

驗證規則見 `crates/primitives/src/genesis.rs` 的 `Genesis::validate`，重點如下：

- chain id 必須是 8017，slot 長度 6 s，每個 epoch 14,400 個 slot
- 委員會至少 512 席，gas limit 在 15M–60M 之間
- 至少 7 個啟動期驗證者，BLS 公鑰不可重複
- 多簽門檻必須過半；升級 timelock 至少 16 天，參數 timelock 至少 2 天
- `alloc` 中所有帳戶餘額必須為 0（無創世分配）
- EIP-4788 與 EIP-2935 的系統合約會自動加入，不可覆寫

## 主網範本 `mainnet.template.json`

已填入的內容：

- 7 個啟動期驗證者的收款地址（`feeRecipient`），均為 CAFECA 提供的地址
- 多簽 9 位簽署者（`governance.owners`），門檻 5-of-9
- 升級 timelock 16 天、參數 timelock 2 天

**這個檔案刻意無法通過驗證，不能直接用來啟動主網**，因為還缺兩項：

1. 每個驗證者的 BLS12-381 公鑰（目前是全零佔位值）。M3 會提供 `boltchain keys` 工具：
   各驗證者營運者在自己的機器上產生 BLS 金鑰，並用 `feeRecipient` 地址的私鑰簽署一份綁定聲明，
   證明這把 BLS 公鑰屬於該地址的持有者。私鑰不離開營運者的機器。
2. `timestamp`：主網啟動時間。

填好之後執行 `boltchain genesis inspect genesis/mainnet.json` 確認通過，再公布 genesis hash。

## Bootstrap 節點

主網 bootstrap 節點使用 `node001.cafeca.io` 到 `node009.cafeca.io`，預設埠 8017（QUIC/UDP 與 TCP）。
客戶端內建的清單是 `/dnsaddr/node00N.cafeca.io`，所以每台主機要在 DNS 發布一筆 TXT 記錄，
把位址和節點的 peer ID 一起公布，例如：

```
_dnsaddr.node001.cafeca.io.  TXT  "dnsaddr=/dns4/node001.cafeca.io/udp/8017/quic-v1/p2p/12D3KooW..."
_dnsaddr.node001.cafeca.io.  TXT  "dnsaddr=/dns4/node001.cafeca.io/tcp/8017/p2p/12D3KooW..."
```

peer ID 由各主機的 `--node-key` 檔決定（`boltchain node-id --node-key <檔案>` 會印出來）。
換主機時只要改 DNS，不必發新版客戶端。
