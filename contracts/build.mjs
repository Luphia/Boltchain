// Compiles the system contracts with solc (solcjs from npm) and writes `out/<Name>.json` with the
// ABI, init code and runtime code. Safe contracts are taken from the published npm artifacts.
//
//   npm ci && node build.mjs          # rebuild
//   node build.mjs --check            # fail if out/ is not what the sources produce (CI)
import fs from 'node:fs';
import path from 'node:path';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
const solc = require('solc');
const here = path.dirname(new URL(import.meta.url).pathname);
const check = process.argv.includes('--check');

// Contracts whose artifacts the node embeds (plus test harnesses).
const WANT = [
  'test/BLSHarness.sol:BLSHarness',
  'src/StakingManager.sol:StakingManager',
  'src/ConsensusRegistry.sol:ConsensusRegistry',
  'src/RewardDistributor.sol:RewardDistributor',
  'src/ParamRegistry.sol:ParamRegistry',
  'src/HistoryRegistry.sol:HistoryRegistry',
  'src/SystemProxy.sol:SystemProxy',
  '@openzeppelin/contracts/governance/TimelockController.sol:TimelockController',
];
const SAFE = {
  Safe: 'Safe.sol/Safe.json',
  SafeProxy: 'proxies/SafeProxy.sol/SafeProxy.json',
  CompatibilityFallbackHandler: 'handler/CompatibilityFallbackHandler.sol/CompatibilityFallbackHandler.json',
};

const sources = {};
for (const f of fs.readdirSync(path.join(here, 'src'))) {
  if (f.endsWith('.sol')) sources[`src/${f}`] = { content: fs.readFileSync(path.join(here, 'src', f), 'utf8') };
}
// Harnesses live in test/ and are compiled alongside.
for (const f of fs.readdirSync(path.join(here, 'test'))) {
  if (f.endsWith('.sol')) sources[`test/${f}`] = { content: fs.readFileSync(path.join(here, 'test', f), 'utf8') };
}
sources['@openzeppelin/contracts/governance/TimelockController.sol'] = {
  content: fs.readFileSync(require.resolve('@openzeppelin/contracts/governance/TimelockController.sol'), 'utf8'),
};

function findImports(p) {
  try {
    return { contents: fs.readFileSync(require.resolve(p), 'utf8') };
  } catch {
    const local = path.join(here, p);
    if (fs.existsSync(local)) return { contents: fs.readFileSync(local, 'utf8') };
    return { error: `not found: ${p}` };
  }
}

const input = {
  language: 'Solidity',
  sources,
  settings: {
    optimizer: { enabled: true, runs: 200 },
    evmVersion: 'prague',
    viaIR: true,
    metadata: { bytecodeHash: 'none', appendCBOR: false },
    outputSelection: { '*': { '*': ['abi', 'evm.bytecode.object', 'evm.deployedBytecode.object'] } },
  },
};
const output = JSON.parse(solc.compile(JSON.stringify(input), { import: findImports }));
let failed = false;
for (const e of output.errors ?? []) {
  if (e.severity === 'error') failed = true;
  console.error(e.formattedMessage);
}
if (failed) process.exit(1);

const outDir = path.join(here, 'out');
fs.mkdirSync(outDir, { recursive: true });
const artifacts = {};
for (const w of WANT) {
  const i = w.lastIndexOf(':');
  const file = w.slice(0, i);
  const name = w.slice(i + 1);
  const c = output.contracts[file]?.[name];
  if (!c) {
    console.error(`missing ${w}`);
    process.exit(1);
  }
  artifacts[name] = {
    abi: c.abi,
    bytecode: '0x' + c.evm.bytecode.object,
    deployedBytecode: '0x' + c.evm.deployedBytecode.object,
  };
}
const safeDir = path.join(path.dirname(require.resolve('@safe-global/safe-contracts/package.json')), 'build/artifacts/contracts');
for (const [name, rel] of Object.entries(SAFE)) {
  const a = JSON.parse(fs.readFileSync(path.join(safeDir, rel), 'utf8'));
  artifacts[name] = { abi: a.abi, bytecode: a.bytecode, deployedBytecode: a.deployedBytecode };
}

let stale = false;
for (const [name, a] of Object.entries(artifacts)) {
  const file = path.join(outDir, `${name}.json`);
  const text = JSON.stringify(a, null, 1) + '\n';
  if (check) {
    if (!fs.existsSync(file) || fs.readFileSync(file, 'utf8') !== text) {
      console.error(`stale artifact: out/${name}.json`);
      stale = true;
    }
  } else {
    fs.writeFileSync(file, text);
    console.log(`out/${name}.json  init ${(a.bytecode.length - 2) / 2} B, runtime ${(a.deployedBytecode.length - 2) / 2} B`);
  }
}
if (stale) process.exit(1);
