# 里程碑

| 里程碑 | 內容 | 驗收標準 | 狀態 |
| --- | --- | --- | --- |
| M0 骨架 | workspace、CI、genesis 格式、資料庫選型 | Osaka statetest 全數通過；CI 在 ARM64 上跑測試 | 程式完成，待 CI 首次執行 |
| M1 單節點鏈 | revm 執行、MPT、MDBX、RPC，暫用單一簽名者出塊 | Foundry 部署合約、MetaMask 轉帳；30M gas / 6 s；樹莓派執行最壞情況區塊 < 2 s | 未開始 |
| M2 IPFS 同步 | IPLD 格式、內嵌 bitswap、gossipsub 公告 | 5 個跟隨節點只靠 IPFS 同步；公告到資料取完 p95 < 1 s | 未開始 |
| M3 BFT 共識 | HotStuff-2、模擬器、DA 投票規則，固定 7 人驗證者集 | 10,000 個故障情境無安全違規；關掉 2 個節點仍持續出塊 | 未開始 |
| M4 PoS | 系統合約、BLS-VRF、512 席抽籤、獎勵、罰沒、啟動期 | 100 個驗證者完成 epoch 輪替；鏈上驗證雙重簽名證據 | 未開始 |
| M5 儲存層 | 檢查點同步、CAR 快照、修剪、歷史分片與抽查 | 新節點從快照起步追上最新區塊 | 未開始 |
| M6 強化與測試網 | peer scoring、NAT 穿透、防罰沒、樹莓派基準 | 樹莓派驗證者 7 天參與率 > 99%，RSS < 1.5 GB | 未開始 |

## M1 待辦（下一步）

- [ ] `store`：libmdbx 資料表（PlainAccounts、PlainStorage、HashedAccounts、HashedStorage、TrieNodes、ChangeSets、Headers、Blockstore）
- [ ] `exec`：區塊執行器（EIP-4788 / EIP-2935 system call、收據、gas 計算、EIP-1559 base fee、EIP-7934 區塊大小）
- [ ] `store`：增量 state root，並用「整棵樹重算」做對照測試
- [ ] `txpool`：admission 規則（拒收 type 3、nonce、餘額、每帳戶 16 筆）
- [ ] `rpc`：eth_chainId、eth_blockNumber、eth_getBalance、eth_call、eth_estimateGas、eth_sendRawTransaction、eth_getTransactionReceipt、eth_getLogs
- [ ] `node`：單一簽名者 devnet 模式
- [ ] 基準：最壞情況區塊（塞滿 BN254 / BLS pairing 呼叫）在 ARM64 上的執行時間
