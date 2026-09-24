# 本地 devnet

`boltchain devnet` 啟動一條只有一個出塊者的開發鏈（M1）。它使用 `genesis/dev.json`：

- chain id **1337**（開發鏈刻意避開 8017，交易才不會被重放到 Boltchain 主網）
- 三個預先注資的開發帳戶，各 10,000 BOLT。這是公開的測試私鑰（與 Foundry/Anvil 預設帳戶相同），**絕對不要在任何有價值的鏈上使用**：

| 地址 | 私鑰 |
| --- | --- |
| `0xf39Fd6e51aad88F6F4ce6aB8827279cffFb92266` | `0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80` |
| `0x70997970C51812dc3A010C7d01b50e0d17dc79C8` | `0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d` |
| `0x3C44CdDdB6a900fa2b585dd299e03d12FA4293BC` | `0x5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a` |

## 啟動

```sh
cargo run --release -p boltchain -- devnet \
  --datadir data/dev --rpc 127.0.0.1:8545 --block-time 6
```

資料會保存在 `--datadir`，重新啟動會接續原本的鏈；刪掉該目錄就能從 genesis 重來。

## 跟隨節點（M2）

出塊者啟動時會印出 `p2p identity peer_id=12D3KooW...`，P2P 預設埠 8017（QUIC 與 TCP）。
在另一個終端機啟動跟隨節點：

```sh
cargo run --release -p boltchain -- follow \
  --datadir data/follower --rpc 127.0.0.1:8546 --p2p-port 8018 \
  --producer <peer id> --bootnode /ip4/127.0.0.1/udp/8017/quic-v1/p2p/<peer id>
```

跟隨節點只透過 IPFS 取得區塊：收到 gossipsub 公告後，用 Bitswap 抓取 body chunk，重新執行並核對 header 與 envelope。
它的 JSON-RPC 可以查詢，`eth_sendRawTransaction` 會轉發給出塊者。跟隨節點也可以當作其他跟隨節點的 `--bootnode`，
不必直接連到出塊者。

節點身分金鑰預設存在 `<datadir>/node.key`；`boltchain node-id --node-key <檔案>` 會印出 peer ID。

## MetaMask

新增自訂網路：

| 欄位 | 值 |
| --- | --- |
| 網路名稱 | Boltchain dev |
| RPC URL | `http://127.0.0.1:8545` |
| Chain ID | `1337` |
| 貨幣符號 | `BOLT` |

再用上表的私鑰匯入帳戶即可轉帳。RPC 已開啟 CORS，瀏覽器裡的 dapp 也能直接連線。

## Foundry

```sh
cast chain-id --rpc-url http://127.0.0.1:8545
cast send --rpc-url http://127.0.0.1:8545 \
  --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
  0x0000000000000000000000000000000000000b0b --value 1ether
forge create --rpc-url http://127.0.0.1:8545 \
  --private-key 0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 \
  src/Counter.sol:Counter --broadcast
```

## 支援的 RPC 方法

`web3_clientVersion`、`net_version`、`net_listening`、`net_peerCount`、
`eth_chainId`、`eth_blockNumber`、`eth_syncing`、`eth_accounts`、`eth_gasPrice`、`eth_maxPriorityFeePerGas`、`eth_feeHistory`、
`eth_getBalance`、`eth_getTransactionCount`、`eth_getCode`、`eth_getStorageAt`、
`eth_call`、`eth_estimateGas`、`eth_sendRawTransaction`、
`eth_getBlockByNumber`、`eth_getBlockByHash`、`eth_getBlockTransactionCountByNumber`、
`eth_getTransactionByHash`、`eth_getTransactionReceipt`、`eth_getBlockReceipts`、`eth_getLogs`。

狀態查詢可以指定 `latest` 或最近 128 個區塊內的區塊號／hash；更早的狀態已修剪，會回傳錯誤。
`eth_blobBaseFee` 一律回傳錯誤，因為 Boltchain 不支援 blob 交易。
