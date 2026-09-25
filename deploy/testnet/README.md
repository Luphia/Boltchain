# 測試網部署

兩種方式，產物相同：同一個 `boltchain` 執行檔與 [`genesis/testnet.json`](../../genesis/testnet.json)。

## 沒有 root：`usermode/`（目前的公開節點就是這樣跑的）

一台主機上跑 N 個節點，全部放在 `~/boltchain`，不需要 sudo。

```sh
# 在主機上：~/boltchain/{bin/boltchain, testnet.json, node.sh, setup.sh, start.sh, stop.sh}
bash ~/boltchain/setup.sh 4 4 2 <公開 IP>   # 4 個節點、每個 4 把驗證者金鑰、每個 2 個挖礦執行緒
~/boltchain/start.sh 4                      # 每個節點在一個 screen 工作階段（bolt-n1…）裡，結束後 5 秒自動重啟
~/boltchain/stop.sh
curl -s 127.0.0.1:9017/health               # 節點 i 的 metrics 在 9016+i
```

- **節點 1 是公開節點**：P2P 8017、JSON-RPC `0.0.0.0:8545`、IPFS 閘道 `0.0.0.0:8080`。
- **其他節點**：P2P 用 8017 + 10(i−1)，RPC 與 metrics 只開在 localhost。
- **金鑰**：帳戶（挖礦收入、質押擁有者）、驗證者金鑰與節點身分都在 `~/boltchain/n<i>/`，建立一次後不會覆寫。
- **重開機**：`setup.sh` 會加一行 `@reboot` crontab，重開機後自動啟動。
- **日誌**：寫在 `~/boltchain/n<i>/node.log`，超過 200 MB 會輪替。

質押（帳戶挖到足夠的 BOLT 之後）：

```sh
for i in 1 2 3 4; do for k in ~/boltchain/n$i/keys/v*.json; do
  ~/boltchain/bin/boltchain wallet stake --wallet ~/boltchain/n$i/wallet.json --key $k --amount 2000 --rpc http://127.0.0.1:$((8544+i))
  ~/boltchain/bin/boltchain wallet set-peer --wallet ~/boltchain/n$i/wallet.json --key $k \
    --node-key ~/boltchain/n$i/data/node.key --rpc http://127.0.0.1:$((8544+i))
done; done
```

## 有 root：systemd（多台主機）

`deploy.sh` 從一台能 SSH 到所有主機的機器執行。主機清單格式見 `hosts.txt.example`。

- `deploy.sh hosts.txt dist/ [ssh-key]`：上傳執行檔並用 `install.sh` 安裝成 systemd 服務（`boltchain.service`）。之後收集每台的 peer id，把完整的 bootnode 清單寫回每台主機並重啟。
- `status.sh hosts.txt [ssh-key] [--logs DIR]`：列出每台的 `/health`，並把最近一小時的日誌抓回本機。
- `stake.sh hosts.txt <每把金鑰的 BOLT>`：用每台主機的挖礦帳戶質押它的驗證者金鑰，並公布歷史資料的 peer。

`dist/` 需要放 `boltchain-linux-x86_64` 及（或）`boltchain-linux-aarch64`，以及 `testnet.json`。
