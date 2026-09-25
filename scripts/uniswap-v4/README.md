# Uniswap v4 部署腳本

把 Uniswap v4 部署到 Boltchain，接著跑一次冒煙測試：

1. 建立原生 BOLT / tUSD 池（手續費 0.30%，初始價格 1 BOLT = 10 tUSD）
2. 經由 PositionManager 加入全區間流動性
3. 經由 UniversalRouter 雙向兌換，並與 V4Quoter 的報價比對

## 部署的合約

| 合約 | 來源 |
| --- | --- |
| PoolManager | `@uniswap/v4-core` 1.0.2 發布的 build（solc 0.8.26，via-IR） |
| PositionManager、PositionDescriptor、StateView、V4Quoter | `@uniswap/v4-periphery` 1.0.3 發布的 build |
| UniversalRouter | `@uniswap/universal-router` 2.1.0 發布的 build（沒有 v2 與 v3，相關參數填 0） |
| Permit2 | 標準的 runtime bytecode（其他鏈上跑的就是這份），只填入本鏈的 chain id 與 EIP-712 domain separator |
| WBOLT | `contracts/TestContracts.sol`，相容 WETH9 |
| tUSD | `contracts/TestContracts.sol`，任何人都能 mint 的測試代幣（每次最多 100 萬），只供測試網使用 |

Permit2 沒辦法用原本的 CREATE2 部署器放到標準位址 `0x000000000022D473…`：那個部署器需要不帶 chain id 的交易，而 Boltchain 只收 EIP-155 交易。
所以 Permit2 部署在一般位址，部署後腳本會檢查兩件事：鏈上的程式碼與標準 runtime 逐位元組相同（兩個 immutable 除外），以及 `DOMAIN_SEPARATOR()` 正確。

## 使用方式

需要 Node.js 18 以上。

```sh
cd scripts/uniswap-v4
npm install
RPC=http://127.0.0.1:8545 WALLET=~/.boltchain-testnet/wallet.json node deploy.mjs
```

- `WALLET`：`boltchain wallet new` 產生的金鑰檔；也可以改用 `PRIVATE_KEY=0x...`
- `LIQUIDITY`：冒煙測試的流動性，以 1e18 為單位，預設 100（約需 32 BOLT 與 320 tUSD）
- `SKIP_SMOKE=1`：只部署合約，不跑冒煙測試

地址寫在 `deployments/<chainId>.json`。中途失敗時重跑即可，已經有程式碼的合約會沿用。

本機 devnet（`genesis/dev.json`，chain 1337）：

```sh
boltchain devnet --datadir /tmp/dev --no-p2p --block-time 1
PRIVATE_KEY=0xac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80 node deploy.mjs
```

## 授權

Uniswap v4 core 採用 BUSL-1.1：可以做非正式環境的使用（例如測試網）。正式環境要等授權轉換日（2027-06-15 或 Uniswap 公布的更早日期）之後，
或是取得 Uniswap 的 Additional Use Grant（見 `v4-core-license-grants.uniswap.eth`）。主網部署前必須先確認授權。
v4-periphery 採用 MIT，UniversalRouter 採用 GPL-2.0-or-later。本目錄只放部署腳本，不附帶 Uniswap 的程式碼；程式碼由 `npm install` 從 npm 取得。
