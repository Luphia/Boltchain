# Boltchain 公開測試網

測試網從 2026-09-25 開始運作。它和主網走同一條路：先用 RandomBOLT 挖礦，質押夠多後由委員會確認檢查點（階段 B），再切換到 PoS。
測試幣只能靠挖礦取得，沒有水龍頭，也沒有創世分配。

測試網的目的是讓問題在真實網路上浮現。遇到任何異常，請到 GitHub 開 issue，並附上節點日誌與 `/health` 的輸出。

## 網路參數

| 項目 | 值 |
| --- | --- |
| Chain ID | 8018 |
| genesis | [`genesis/testnet.json`](../genesis/testnet.json)，區塊雜湊 `0x4036621660f1990718ec2c59c821657ac17b91a4fa620a654e0c9bc8db137a99` |
| 代幣 | BOLT（測試用，沒有價值） |
| 挖礦 | RandomBOLT，目標 12 秒一塊，ASERT 半衰期 1 小時 |
| epoch | 600 塊（挖礦階段約 2 小時；PoS 階段 6 秒一塊，約 1 小時） |
| 階段 B 門檻 | 8 位驗證者、共 20,000 BOLT，維持 3 個 epoch |
| PoS 門檻 | 16 位驗證者、共 100,000 BOLT，維持 3 個 epoch |
| 最低質押 | 64 BOLT |
| 歷史保留窗口 | 3 個 epoch（更舊的由被指派的驗證者保存並接受抽查） |

## 公開端點

| 用途 | 位址 |
| --- | --- |
| Bootnode（QUIC） | `/ip4/211.22.118.149/udp/8017/quic-v1/p2p/12D3KooWCFZiVi5pkzNA83AZqaUevCbi5fYULiEGtmz8FSvMuhDo` |
| Bootnode（TCP） | `/ip4/211.22.118.149/tcp/8017/p2p/12D3KooWCFZiVi5pkzNA83AZqaUevCbi5fYULiEGtmz8FSvMuhDo` |
| JSON-RPC | `http://211.22.118.149:8545` |
| IPFS 閘道 | `http://211.22.118.149:8080/ipfs/<cid>?format=car`、`http://211.22.118.149:8080/history/epoch/<n>.car` |

MetaMask：新增網路。網路名稱填 `Boltchain Testnet`，RPC 填 `http://211.22.118.149:8545`，Chain ID 填 `8018`，貨幣符號填 `BOLT`。

## 取得執行檔

從原始碼編譯需要 Rust 1.95、cmake、clang（libclang）與 C++ 編譯器：

```sh
git clone https://github.com/Luphia/Boltchain && cd Boltchain
cargo build --release -p boltchain
./target/release/boltchain genesis inspect genesis/testnet.json   # 區塊雜湊應與上表一致
```

發布 tag 之後，GitHub Releases 會有 Linux x86_64、Linux ARM64（樹莓派）與 macOS ARM64 的執行檔。

## 挖礦

```sh
B=./target/release/boltchain
BOOT=/ip4/211.22.118.149/udp/8017/quic-v1/p2p/12D3KooWCFZiVi5pkzNA83AZqaUevCbi5fYULiEGtmz8FSvMuhDo
$B wallet new --out ~/.boltchain-testnet/wallet.json            # 記下地址；檔案權限 0600，請備份
$B mine --genesis genesis/testnet.json --datadir ~/.boltchain-testnet/data \
  --bootnode $BOOT --beneficiary <你的地址> \
  --randomx-fast --mining-threads 4 --metrics 127.0.0.1:9017
```

- `--randomx-fast`：使用 2 GiB 資料集，雜湊速度約快 8 倍；啟動時需要約 1 分鐘初始化。記憶體少於 4 GiB 的機器請拿掉這個選項。
- 家用網路請在路由器上把 UDP 與 TCP 8017 轉發到這台機器，或設定 `--p2p-port`。NAT 穿透是下一階段的工作。
- 節點每分鐘會在日誌印一行 `status`，內容包括高度、peer 數、雜湊率與已挖到的區塊數。

查詢餘額：`$B wallet balance --wallet ~/.boltchain-testnet/wallet.json`（預設連本機 RPC `http://127.0.0.1:8545`）。

## 成為驗證者

帳戶挖到至少 64 BOLT 之後：

```sh
$B keys new --out ~/.boltchain-testnet/validator.json
$B wallet stake --wallet ~/.boltchain-testnet/wallet.json --key ~/.boltchain-testnet/validator.json --amount 100
$B wallet set-peer --wallet ~/.boltchain-testnet/wallet.json --key ~/.boltchain-testnet/validator.json \
  --node-key ~/.boltchain-testnet/data/node.key
```

- `set-peer` 公布你的節點是這位驗證者保存歷史資料的地方。沒有公布的驗證者被抽查時一律算失敗，每次罰 1% 質押。
- 之後用 `validator` 取代 `mine` 重新啟動節點。參數相同，另外加上 `--key ~/.boltchain-testnet/validator.json`；如果還要繼續挖礦，加上 `--mine`。

## 從快照加入（不必從 genesis 同步）

先向可信的來源取得最近一個 epoch 結尾區塊的高度與雜湊，例如自己的另一個節點，或 `eth_getBlockByNumber`，然後：

```sh
$B validator --genesis genesis/testnet.json --datadir ~/.boltchain-testnet/data2 --bootnode $BOOT \
  --checkpoint <高度>:<雜湊>
```

## 監看與回報

- `curl 127.0.0.1:9017/health`：高度、階段、peer 數、最終區塊、快照高度。
- `curl 127.0.0.1:9017/metrics`：Prometheus 格式，包括匯入、重組、被拒提案、Bitswap、抽查、修剪等計數。
- 回報問題時請附上：`/health` 輸出、日誌中的 `WARN` 與 `ERROR`、作業系統與 CPU、網路環境（是否在 NAT 後面）。

## 已知限制（測試網第一階段）

- 沒有 NAT 穿透與 relay：在 NAT 後面、又沒有做連接埠轉發的節點只能對外連線，別人連不進來。
- 沒有交易 gossip：交易要送到會出塊的節點。公開 RPC 那台節點會把交易放進自己的區塊。
- peer 評分與速率限制還沒做。
- 測試網可能因為修正而重置，重置時會更新本頁的 genesis 雜湊。
