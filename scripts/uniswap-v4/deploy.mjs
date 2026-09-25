// Deploys Uniswap v4 to a Boltchain network, then runs a smoke test: a native BOLT / tUSD pool
// is created, liquidity is added through PositionManager and swaps go through UniversalRouter.
//
//   npm install
//   RPC=http://127.0.0.1:8545 WALLET=~/.boltchain-testnet/wallet.json node deploy.mjs
//
// Environment:
//   RPC          JSON-RPC endpoint (default http://127.0.0.1:8545)
//   WALLET       `boltchain wallet new` key file, or PRIVATE_KEY=0x...
//   LIQUIDITY    liquidity of the smoke-test position, in units of 1e18 (default 100)
//   SKIP_SMOKE   set to skip the pool / liquidity / swap test
//
// Addresses are written to deployments/<chainId>.json; a re-run reuses contracts that already
// have code there, so an interrupted deployment can simply be restarted.
//
// Uniswap v4 core is BUSL-1.1: non-production use (testnets) is allowed; production use on a new
// chain needs an Additional Use Grant from Uniswap (see README.md).

import { ethers } from "ethers";
import solc from "solc";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { fileURLToPath } from "node:url";

const here = path.dirname(fileURLToPath(import.meta.url));
const nm = (p) => path.join(here, "node_modules", p);
const log = (...a) => console.log(new Date().toISOString().slice(11, 19), ...a);

// ---------------------------------------------------------------------------------------------
// Artifacts

function foundry(p) {
  const j = JSON.parse(fs.readFileSync(nm(p), "utf8"));
  return { abi: j.abi, bytecode: j.bytecode.object ?? j.bytecode };
}

const ART = {
  PoolManager: foundry("@uniswap/v4-core/out/PoolManager.sol/PoolManager.json"),
  PositionDescriptor: foundry(
    "@uniswap/v4-periphery/foundry-out/PositionDescriptor.sol/PositionDescriptor.json",
  ),
  PositionManager: foundry(
    "@uniswap/v4-periphery/foundry-out/PositionManager.sol/PositionManager.json",
  ),
  StateView: foundry("@uniswap/v4-periphery/foundry-out/StateView.sol/StateView.json"),
  V4Quoter: foundry("@uniswap/v4-periphery/foundry-out/V4Quoter.sol/V4Quoter.json"),
  UniversalRouter: foundry(
    "@uniswap/universal-router/artifacts/contracts/UniversalRouter.sol/UniversalRouter.json",
  ),
};

// WBOLT and the test token are compiled from source (contracts/TestContracts.sol, solmate).
//
// Permit2 is deployed from its canonical runtime code (the build shipped with v4-periphery, which
// is what Ethereum and other chains run). Its only chain-specific parts are two immutables its
// constructor sets: the chain id and the EIP-712 domain separator (which includes the contract's
// own address). A 13-byte init code returns the canonical runtime with those two words filled in
// for this chain and address, i.e. exactly what the canonical creation code would leave behind.
const PERMIT2_DIR = nm("@uniswap/v4-periphery/lib/permit2");

function solcCompile(sources, read, remappings = []) {
  const input = {
    language: "Solidity",
    sources,
    settings: {
      viaIR: true,
      optimizer: { enabled: true, runs: 1_000_000 },
      metadata: { bytecodeHash: "none" },
      remappings,
      outputSelection: {
        "*": { "*": ["abi", "evm.bytecode.object", "evm.deployedBytecode"] },
      },
    },
  };
  const out = JSON.parse(solc.compile(JSON.stringify(input), { import: read }));
  const errors = (out.errors ?? []).filter((e) => e.severity === "error");
  if (errors.length) throw new Error(errors.map((e) => e.formattedMessage).join("\n"));
  return (file, name) => {
    const c = out.contracts[file][name];
    return {
      abi: c.abi,
      bytecode: "0x" + c.evm.bytecode.object,
      runtime: "0x" + c.evm.deployedBytecode.object,
      immutables: c.evm.deployedBytecode.immutableReferences ?? {},
    };
  };
}

function compileLocal() {
  const cacheFile = path.join(here, "build", "solc-0.8.17.json");
  if (fs.existsSync(cacheFile)) return JSON.parse(fs.readFileSync(cacheFile, "utf8"));
  log("compiling WBOLT and TestToken with solc", solc.version());
  const fromPermit2 = (p) =>
    fs.existsSync(path.join(PERMIT2_DIR, p))
      ? { contents: fs.readFileSync(path.join(PERMIT2_DIR, p), "utf8") }
      : { error: `not found: ${p}` };
  const local = solcCompile(
    {
      "TestContracts.sol": {
        content: fs.readFileSync(path.join(here, "contracts/TestContracts.sol"), "utf8"),
      },
    },
    fromPermit2,
    ["solmate/=lib/solmate/"],
  );
  const res = {
    WBOLT: local("TestContracts.sol", "WBOLT"),
    TestToken: local("TestContracts.sol", "TestToken"),
  };
  fs.mkdirSync(path.dirname(cacheFile), { recursive: true });
  fs.writeFileSync(cacheFile, JSON.stringify(res));
  return res;
}

const PERMIT2_CHAIN_ID_AT = 6945; // PUSH32 after CHAINID in the canonical runtime
const PERMIT2_SEPARATOR_AT = 6983; // PUSH32 of the cached domain separator

function canonicalPermit2Runtime() {
  const src = fs.readFileSync(path.join(PERMIT2_DIR, "test/utils/DeployPermit2.sol"), "utf8");
  const code = ethers.getBytes("0x" + src.match(/hex"([0-9a-fA-F]+)"/)[1]);
  if (
    code.length !== 9152 ||
    code[PERMIT2_CHAIN_ID_AT - 2] !== 0x46 ||
    code[PERMIT2_CHAIN_ID_AT - 1] !== 0x7f
  )
    throw new Error("unexpected Permit2 runtime layout");
  return code;
}

function permit2Separator(chainId, address) {
  const TYPE = ethers.id("EIP712Domain(string name,uint256 chainId,address verifyingContract)");
  return ethers.keccak256(
    ethers.AbiCoder.defaultAbiCoder().encode(
      ["bytes32", "bytes32", "uint256", "address"],
      [TYPE, ethers.id("Permit2"), chainId, address],
    ),
  );
}

function permit2InitCode(chainId, address) {
  const code = canonicalPermit2Runtime().slice();
  code.set(ethers.getBytes(ethers.toBeHex(chainId, 32)), PERMIT2_CHAIN_ID_AT);
  code.set(ethers.getBytes(permit2Separator(chainId, address)), PERMIT2_SEPARATOR_AT);
  const len = code.length.toString(16).padStart(4, "0");
  // PUSH2 len, DUP1, PUSH2 13, PUSH1 0, CODECOPY, PUSH1 0, RETURN
  return {
    init: "0x61" + len + "8061000d6000396000f3" + ethers.hexlify(code).slice(2),
    runtime: ethers.hexlify(code),
  };
}

const PERMIT2_ABI = [
  "function DOMAIN_SEPARATOR() view returns (bytes32)",
  "function approve(address token, address spender, uint160 amount, uint48 expiration)",
  "function allowance(address user, address token, address spender) view returns (uint160 amount, uint48 expiration, uint48 nonce)",
];

// ---------------------------------------------------------------------------------------------
// Wallet, network, bookkeeping

function signer(provider) {
  let key = process.env.PRIVATE_KEY;
  if (!key && process.env.WALLET) {
    const f = process.env.WALLET.replace(/^~(?=\/)/, os.homedir());
    key = JSON.parse(fs.readFileSync(f, "utf8")).privateKey;
  }
  if (!key) throw new Error("set WALLET=<boltchain wallet file> or PRIVATE_KEY=0x...");
  return new ethers.Wallet(key, provider);
}

const provider = new ethers.JsonRpcProvider(process.env.RPC ?? "http://127.0.0.1:8545", undefined, {
  staticNetwork: true,
  batchMaxCount: 1,
  pollingInterval: 1000,
});
const wallet = signer(provider);
const me = wallet.address;
const { chainId } = await provider.getNetwork();
const deployFile = path.join(here, "deployments", `${chainId}.json`);
const book = fs.existsSync(deployFile) ? JSON.parse(fs.readFileSync(deployFile, "utf8")) : {};
book.chainId = Number(chainId);
book.contracts ??= {};
const save = () => {
  fs.mkdirSync(path.dirname(deployFile), { recursive: true });
  fs.writeFileSync(deployFile, JSON.stringify(book, null, 2) + "\n");
};

log(
  `chain ${chainId}, deployer ${me}, balance ${ethers.formatEther(await provider.getBalance(me))} BOLT`,
);

async function send(label, txp) {
  const tx = await txp;
  const r = await tx.wait();
  if (r.status !== 1) throw new Error(`${label}: reverted (${tx.hash})`);
  log(`${label}: block ${r.blockNumber}, gas ${r.gasUsed}`);
  return r;
}

async function deploy(name, art, args = [], opts = {}) {
  const known = book.contracts[name];
  if (known && (await provider.getCode(known)) !== "0x") {
    log(`${name}: already at ${known}`);
    return new ethers.Contract(known, art.abi, wallet);
  }
  const f = new ethers.ContractFactory(art.abi, art.bytecode, wallet);
  const c = await f.deploy(...args, opts);
  const r = await c.deploymentTransaction().wait();
  const addr = await c.getAddress();
  const size = (ethers.getBytes(await provider.getCode(addr)).length / 1024).toFixed(1);
  log(`${name}: ${addr} (block ${r.blockNumber}, gas ${r.gasUsed}, ${size} KiB)`);
  book.contracts[name] = addr;
  save();
  return new ethers.Contract(addr, art.abi, wallet);
}

// ---------------------------------------------------------------------------------------------
// Deployment

const local = compileLocal();
const ZERO = ethers.ZeroAddress;

async function deployPermit2() {
  const known = book.contracts.Permit2;
  if (!known || (await provider.getCode(known)) === "0x") {
    const nonce = await provider.getTransactionCount(me, "pending");
    const addr = ethers.getCreateAddress({ from: me, nonce });
    const { init } = permit2InitCode(chainId, addr);
    const r = await send("Permit2", wallet.sendTransaction({ data: init, nonce }));
    if (r.contractAddress !== addr)
      throw new Error(`Permit2 landed at ${r.contractAddress}, expected ${addr}`);
    book.contracts.Permit2 = addr;
    save();
  }
  const addr = book.contracts.Permit2;
  const c = new ethers.Contract(addr, PERMIT2_ABI, wallet);
  if ((await provider.getCode(addr)) !== permit2InitCode(chainId, addr).runtime)
    throw new Error("Permit2 code differs from the canonical runtime");
  if ((await c.DOMAIN_SEPARATOR()) !== permit2Separator(chainId, addr))
    throw new Error("Permit2 domain separator mismatch");
  log(`Permit2: ${addr} (canonical runtime, domain separator for chain ${chainId})`);
  return c;
}
const permit2 = await deployPermit2();
const wbolt = await deploy("WBOLT", local.WBOLT);
const poolManager = await deploy("PoolManager", ART.PoolManager, [me]);
const pm = await poolManager.getAddress();
const descriptor = await deploy("PositionDescriptor", ART.PositionDescriptor, [
  pm,
  await wbolt.getAddress(),
  ethers.encodeBytes32String("BOLT"),
]);
const posm = await deploy("PositionManager", ART.PositionManager, [
  pm,
  await permit2.getAddress(),
  300_000, // gas limit for unsubscribe notifications (as on Ethereum)
  await descriptor.getAddress(),
  await wbolt.getAddress(),
]);
const stateView = await deploy("StateView", ART.StateView, [pm]);
const quoter = await deploy("V4Quoter", ART.V4Quoter, [pm]);
const router = await deploy("UniversalRouter", ART.UniversalRouter, [
  {
    permit2: await permit2.getAddress(),
    weth9: await wbolt.getAddress(),
    v2Factory: ZERO, // no Uniswap v2 / v3 on Boltchain
    v3Factory: ZERO,
    pairInitCodeHash: ethers.ZeroHash,
    poolInitCodeHash: ethers.ZeroHash,
    v4PoolManager: pm,
    v3NFTPositionManager: ZERO,
    v4PositionManager: await posm.getAddress(),
    spokePool: ZERO,
  },
]);
book.versions = {
  "@uniswap/v4-core": "1.0.2",
  "@uniswap/v4-periphery": "1.0.3",
  "@uniswap/universal-router": "2.1.0",
  permit2: "canonical runtime (v4-periphery lib/permit2), chain-specific immutables",
};
save();

if (process.env.SKIP_SMOKE) process.exit(0);

// ---------------------------------------------------------------------------------------------
// Smoke test: native BOLT / tUSD, 0.30 % fee, 1 BOLT = 10 tUSD, full-range position, swaps both
// ways through UniversalRouter, checked against V4Quoter.

const tusd = await deploy("tUSD", local.TestToken, ["Test USD", "tUSD", 18]);
const T = await tusd.getAddress();
const key = { currency0: ZERO, currency1: T, fee: 3000, tickSpacing: 60, hooks: ZERO };
const KEY_T =
  "tuple(address currency0,address currency1,uint24 fee,int24 tickSpacing,address hooks)";
const coder = ethers.AbiCoder.defaultAbiCoder();
const poolId = ethers.keccak256(coder.encode([KEY_T], [key]));
const POOL = "BOLT/tUSD 0.3%";
book.pools ??= {};
book.pools[POOL] = { ...book.pools[POOL], poolId, key };
save();

const isqrt = (n) => {
  if (n < 2n) return n;
  let x = n,
    y = (x + 1n) / 2n;
  while (y < x) [x, y] = [y, (y + n / y) / 2n];
  return x;
};
const Q96 = 2n ** 96n;
const sqrtPriceX96 = isqrt(10n * Q96 * Q96); // price = tUSD per BOLT = 10

const MAX160 = 2n ** 160n - 1n;
const MAX48 = 2n ** 48n - 1n;
const deadline = () => BigInt(Math.floor(Date.now() / 1000) + 600);
const bal = async () => ({
  bolt: ethers.formatEther(await provider.getBalance(me)),
  tusd: ethers.formatEther(await tusd.balanceOf(me)),
});

if ((await tusd.balanceOf(me)) < ethers.parseEther("5000"))
  await send("mint 10,000 tUSD", tusd.mint(me, ethers.parseEther("10000")));
if ((await tusd.allowance(me, await permit2.getAddress())) < ethers.MaxUint256 / 2n)
  await send("tUSD approve Permit2", tusd.approve(await permit2.getAddress(), ethers.MaxUint256));
for (const [who, c] of [
  ["PositionManager", posm],
  ["UniversalRouter", router],
])
  if ((await permit2.allowance(me, T, await c.getAddress())).amount < MAX160 / 2n)
    await send(
      `Permit2 allowance for ${who}`,
      permit2.approve(T, await c.getAddress(), MAX160, MAX48),
    );

const [slot0Price] = await stateView.getSlot0(poolId);
if (slot0Price === 0n) await send("initialize pool", poolManager.initialize(key, sqrtPriceX96));
else log("pool already initialized");

// Add full-range liquidity: MINT_POSITION, SETTLE_PAIR, SWEEP (refund unused native BOLT).
const A = {
  MINT_POSITION: 0x02,
  SWAP_EXACT_IN_SINGLE: 0x06,
  SETTLE_ALL: 0x0c,
  SETTLE_PAIR: 0x0d,
  TAKE_ALL: 0x0f,
  SWEEP: 0x14,
};
const actions = (...xs) => ethers.hexlify(Uint8Array.from(xs));
const liquidity = ethers.parseEther(process.env.LIQUIDITY ?? "100");
const tickLower = -887220,
  tickUpper = 887220; // full range for tick spacing 60
// Full range: amount0 ≈ L / sqrtP, amount1 ≈ L · sqrtP (sqrtP = √10 ≈ 3.16); 10 % headroom.
const max0 = (((liquidity * 1000n) / 3162n) * 11n) / 10n + 1n;
const max1 = (((liquidity * 3163n) / 1000n) * 11n) / 10n + 1n;
if (book.pools[POOL].smokePosition === undefined) {
  const before = await bal();
  const mintParams = [
    coder.encode(
      [KEY_T, "int24", "int24", "uint256", "uint128", "uint128", "address", "bytes"],
      [key, tickLower, tickUpper, liquidity, max0, max1, me, "0x"],
    ),
    coder.encode(["address", "address"], [ZERO, T]),
    coder.encode(["address", "address"], [ZERO, me]),
  ];
  const r = await send(
    "add liquidity (PositionManager)",
    posm.modifyLiquidities(
      coder.encode(
        ["bytes", "bytes[]"],
        [actions(A.MINT_POSITION, A.SETTLE_PAIR, A.SWEEP), mintParams],
      ),
      deadline(),
      { value: max0 },
    ),
  );
  const minted = r.logs
    .filter((l) => l.address === posm.target)
    .map((l) => posm.interface.parseLog(l))
    .find((e) => e?.name === "Transfer" && e.args.from === ZERO);
  const tokenId = minted.args.id;
  log(
    `position #${tokenId}, owner ${await posm.ownerOf(tokenId)}, liquidity ${ethers.formatEther(await posm.getPositionLiquidity(tokenId))}`,
  );
  log("balances before / after:", before, await bal());
  book.pools[POOL].smokePosition = Number(tokenId);
  save();
} else log(`position #${book.pools[POOL].smokePosition} already added`);

// Swaps through UniversalRouter (command V4_SWAP = 0x10).
const EXACT_IN_T = `tuple(${KEY_T} poolKey,bool zeroForOne,uint128 amountIn,uint128 amountOutMinimum,bytes hookData)`;
async function swap(zeroForOne, amountIn) {
  const [quoted] = await quoter.quoteExactInputSingle.staticCall({
    poolKey: key,
    zeroForOne,
    exactAmount: amountIn,
    hookData: "0x",
  });
  const [cin, cout] = zeroForOne ? [ZERO, T] : [T, ZERO];
  const params = [
    coder.encode(
      [EXACT_IN_T],
      [
        {
          poolKey: key,
          zeroForOne,
          amountIn,
          amountOutMinimum: (quoted * 99n) / 100n,
          hookData: "0x",
        },
      ],
    ),
    coder.encode(["address", "uint256"], [cin, amountIn]),
    coder.encode(["address", "uint256"], [cout, (quoted * 99n) / 100n]),
  ];
  const input = coder.encode(
    ["bytes", "bytes[]"],
    [actions(A.SWAP_EXACT_IN_SINGLE, A.SETTLE_ALL, A.TAKE_ALL), params],
  );
  const b0 = await bal();
  await send(
    `swap ${ethers.formatEther(amountIn)} ${zeroForOne ? "BOLT -> tUSD" : "tUSD -> BOLT"} (quoted ${ethers.formatEther(quoted)})`,
    router["execute(bytes,bytes[],uint256)"]("0x10", [input], deadline(), {
      value: zeroForOne ? amountIn : 0n,
    }),
  );
  log("  balances", b0, "->", await bal());
  return quoted;
}
const out1 = await swap(true, ethers.parseEther("1"));
await swap(false, out1 / 2n);

const [sp, tick, , lpFee] = await stateView.getSlot0(poolId);
const price = Number((sp * sp * 10n ** 6n) / (Q96 * Q96)) / 1e6;
log(
  `pool ${poolId}: tick ${tick}, lpFee ${lpFee}, price ${price} tUSD/BOLT, liquidity ${ethers.formatEther(await stateView.getLiquidity(poolId))}`,
);
log("done; addresses in", path.relative(process.cwd(), deployFile));
