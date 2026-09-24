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
