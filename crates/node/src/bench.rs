//! Block execution benchmarks.
//!
//! * `transfers`: a block filled with 21,000-gas transfers (the throughput target: 30M gas / 6 s).
//! * `pairing` / `bls-pairing`: the worst case the plan worries about, a block whose gas is spent
//!   almost entirely on the BN254 (0x08) or BLS12-381 (0x0f) pairing precompile. On Raspberry Pi
//!   class hardware a validator must re-execute it in under 2 s to leave room in the 2.5 s voting
//!   window.
//!
//! Each run produces blocks on a throwaway datadir with the dev genesis and reports execution
//! plus commit time (MDBX durable commit included).

use alloy_consensus::{SignableTransaction, TxEip1559, TxEnvelope};
use alloy_network::TxSignerSync;
use alloy_primitives::{Address, Bytes, TxKind, U256, hex};
use alloy_signer_local::PrivateKeySigner;
use anyhow::{Result, bail};
use bolt_chain::Chain;
use bolt_primitives::Genesis;
use std::time::Instant;

/// Which benchmark to run.
#[derive(Debug, Clone, Copy, clap::ValueEnum)]
pub enum Kind {
    /// Plain transfers.
    Transfers,
    /// BN254 pairing precompile (0x08).
    Pairing,
    /// BLS12-381 pairing precompile (0x0f, EIP-2537).
    BlsPairing,
    /// Both.
    All,
}

/// Benchmark options.
#[derive(Debug, clap::Args)]
pub struct BenchArgs {
    /// Which benchmark.
    #[arg(value_enum, default_value_t = Kind::All)]
    pub kind: Kind,
    /// Blocks per benchmark (the first is reported separately as a warm-up).
    #[arg(long, default_value_t = 4)]
    pub blocks: u64,
}

const DEV_GENESIS: &str = include_str!("../../../genesis/dev.json");
const DEV_KEYS: [&str; 3] = [
    "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
    "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
    "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a",
];

/// BN254 pairing input: `(G1, G2)` followed by `(-G1, G2)`, which pairs to one.
/// Encoded per EIP-197 (G2 coordinates imaginary part first).
pub fn bn254_pair_input(pairs: usize) -> Bytes {
    const G1: &str = concat!(
        "0000000000000000000000000000000000000000000000000000000000000001",
        "0000000000000000000000000000000000000000000000000000000000000002",
    );
    const NEG_G1: &str = concat!(
        "0000000000000000000000000000000000000000000000000000000000000001",
        "30644e72e131a029b85045b68181585d97816a916871ca8d3c208c16d87cfd45",
    );
    const G2: &str = concat!(
        "198e9393920d483a7260bfb731fb5d25f1aa493335a9e71297e485b7aef312c2",
        "1800deef121f1e76426a00665e5c4479674322d4f75edadd46debd5cd992f6ed",
        "090689d0585ff075ec9e99ad690c3395bc4b313370b38ef355acdadcd122975b",
        "12c85ea5db8c6deb4aab71808dcb408fe3d1e7690c43d37b4ce6cc0166fa7daa",
    );
    let mut out = Vec::with_capacity(pairs * 192);
    for i in 0..pairs {
        let g1 = if i % 2 == 0 { G1 } else { NEG_G1 };
        out.extend(hex::decode(g1).expect("const hex"));
        out.extend(hex::decode(G2).expect("const hex"));
    }
    out.into()
}

/// BLS12-381 pairing input (EIP-2537 encoding): `(G1, G2)` then `(-G1, G2)`, which pairs to one.
pub fn bls_pair_input(pairs: usize) -> Bytes {
    const G1: &str = concat!(
        "0000000000000000000000000000000017f1d3a73197d7942695638c4fa9ac0fc3688c4f9774b905a14e3a3f171bac586c55e83ff97a1aeffb3af00adb22c6bb",
        "0000000000000000000000000000000008b3f481e3aaa0f1a09e30ed741d8ae4fcf5e095d5d00af600db18cb2c04b3edd03cc744a2888ae40caa232946c5e7e1",
    );
    const NEG_G1: &str = concat!(
        "0000000000000000000000000000000017f1d3a73197d7942695638c4fa9ac0fc3688c4f9774b905a14e3a3f171bac586c55e83ff97a1aeffb3af00adb22c6bb",
        "00000000000000000000000000000000114d1d6855d545a8aa7d76c8cf2e21f267816aef1db507c96655b9d5caac42364e6f38ba0ecb751bad54dcd6b939c2ca",
    );
    const G2: &str = concat!(
        "00000000000000000000000000000000024aa2b2f08f0a91260805272dc51051c6e47ad4fa403b02b4510b647ae3d1770bac0326a805bbefd48056c8c121bdb8",
        "0000000000000000000000000000000013e02b6052719f607dacd3a088274f65596bd0d09920b61ab5da61bbdc7f5049334cf11213945d57e5ac7d055d042b7e",
        "000000000000000000000000000000000ce5d527727d6e118cc9cdc6da2e351aadfd9baa8cbdd3a76d429a695160d12c923ac9cc3baca289e193548608b82801",
        "000000000000000000000000000000000606c4a02ea734cc32acd2b02bc28b99cb3e287e85a763af267492ab572e99ab3f370d275cec1da1aaa9075ff05f79be",
    );
    let mut out = Vec::with_capacity(pairs * 384);
    for i in 0..pairs {
        let g1 = if i % 2 == 0 { G1 } else { NEG_G1 };
        out.extend(hex::decode(g1).expect("const hex"));
        out.extend(hex::decode(G2).expect("const hex"));
    }
    out.into()
}

fn pairing_block(
    keys: &[PrivateKeySigner],
    cid: u64,
    nonces: &mut [u64],
    precompile: u8,
    input: fn(usize) -> Bytes,
) -> Vec<(TxEnvelope, Address)> {
    // One transaction near the EIP-7825 cap plus one using most of the rest of the block.
    let mut out = Vec::new();
    for (k, pairs, gas) in [(0usize, 440usize, 16_700_000u64), (1, 340, 13_250_000)] {
        let tx =
            sign(&keys[k], cid, nonces[k], Address::with_last_byte(precompile), input(pairs), gas);
        nonces[k] += 1;
        out.push((tx, keys[k].address()));
    }
    out
}

fn sign(
    k: &PrivateKeySigner,
    chain_id: u64,
    nonce: u64,
    to: Address,
    input: Bytes,
    gas: u64,
) -> TxEnvelope {
    let mut t = TxEip1559 {
        chain_id,
        nonce,
        gas_limit: gas,
        max_fee_per_gas: 100_000_000_000,
        max_priority_fee_per_gas: 1,
        to: TxKind::Call(to),
        value: U256::ZERO,
        input,
        ..Default::default()
    };
    let sig = k.sign_transaction_sync(&mut t).expect("signing");
    t.into_signed(sig).into()
}

struct Report {
    name: &'static str,
    /// Producer: execute + commit.
    ms: Vec<f64>,
    /// Validator: recover senders + re-execute + verify header + commit.
    import_ms: Vec<f64>,
    gas: Vec<u64>,
    txs: Vec<usize>,
}

impl Report {
    fn stats(v: &[f64]) -> (f64, f64) {
        let steady: Vec<f64> = v.iter().skip(1).copied().collect();
        let worst = steady.iter().copied().fold(0.0, f64::max);
        let avg = steady.iter().sum::<f64>() / steady.len().max(1) as f64;
        (avg, worst)
    }

    fn worst_import(&self) -> f64 {
        Self::stats(&self.import_ms).1
    }

    fn print(&self) {
        let (avg, worst) = Self::stats(&self.ms);
        let (iavg, iworst) = Self::stats(&self.import_ms);
        let gas = self.gas.iter().skip(1).sum::<u64>() as f64 / (self.gas.len().max(2) - 1) as f64;
        println!(
            "{:<10} {:>5.1} Mgas/block, {:>4} txs | produce avg {:>7.1} ms, worst {:>7.1} ms | import avg {:>7.1} ms, worst {:>7.1} ms | {:>6.1} Mgas/s (import)",
            self.name,
            gas / 1e6,
            self.txs.last().copied().unwrap_or(0),
            avg,
            worst,
            iavg,
            iworst,
            gas / 1e6 / (iavg / 1000.0).max(1e-9),
        );
    }
}

fn run_one(
    name: &'static str,
    blocks: u64,
    mk: impl Fn(&[PrivateKeySigner], u64, &mut [u64]) -> Vec<(TxEnvelope, Address)>,
) -> Result<Report> {
    let genesis = Genesis::from_json(DEV_GENESIS)?;
    let dir = tempfile::tempdir()?;
    let chain = Chain::open(dir.path(), &genesis)?;
    let dir2 = tempfile::tempdir()?;
    let validator = Chain::open(dir2.path(), &genesis)?;
    let keys: Vec<PrivateKeySigner> =
        DEV_KEYS.iter().map(|k| k.parse().expect("dev key")).collect();
    let mut nonces = vec![0u64; keys.len()];
    let mut report = Report { name, ms: vec![], import_ms: vec![], gas: vec![], txs: vec![] };
    for i in 0..blocks.max(2) {
        let cands = mk(&keys, genesis.config.chain_id, &mut nonces);
        let txs: Vec<TxEnvelope> = cands.iter().map(|(t, _)| t.clone()).collect();
        let t = Instant::now();
        let built = chain.build_block(cands, 1_000 + i * 6, Address::ZERO)?;
        report.ms.push(t.elapsed().as_secs_f64() * 1000.0);
        if let Some((_, why, _)) = built.rejected.first() {
            bail!("{name}: unexpected rejection: {why}");
        }
        let t = Instant::now();
        let hash = validator.import_block(&built.header, txs)?;
        report.import_ms.push(t.elapsed().as_secs_f64() * 1000.0);
        if hash != built.hash {
            bail!("{name}: validator computed a different block hash");
        }
        report.gas.push(built.header.gas_used);
        report.txs.push(built.included.len());
    }
    Ok(report)
}

/// Runs the requested benchmarks and prints a summary.
pub fn run(args: BenchArgs) -> Result<()> {
    println!(
        "arch {} | {} blocks each (first = warm-up)",
        std::env::consts::ARCH,
        args.blocks.max(2)
    );
    if matches!(args.kind, Kind::Transfers | Kind::All) {
        let r = run_one("transfers", args.blocks, |keys, cid, nonces| {
            let per_block = (bolt_primitives::params::DEFAULT_GAS_LIMIT / 21_000) as usize;
            (0..per_block)
                .map(|i| {
                    let k = i % keys.len();
                    let tx = sign(
                        &keys[k],
                        cid,
                        nonces[k],
                        Address::with_last_byte((i % 250) as u8 + 1),
                        Bytes::new(),
                        21_000,
                    );
                    nonces[k] += 1;
                    (tx, keys[k].address())
                })
                .collect()
        })?;
        r.print();
    }
    let mut worst = Vec::new();
    for (kind, name, pc, input) in [
        (Kind::Pairing, "bn254", 8u8, bn254_pair_input as fn(usize) -> Bytes),
        (Kind::BlsPairing, "bls12-381", 0x0f, bls_pair_input),
    ] {
        if matches!(args.kind, Kind::All)
            || std::mem::discriminant(&args.kind) == std::mem::discriminant(&kind)
        {
            let r = run_one(name, args.blocks, |keys, cid, nonces| {
                pairing_block(keys, cid, nonces, pc, input)
            })?;
            r.print();
            worst.push((name, r.worst_import()));
        }
    }
    for (name, ms) in worst {
        println!(
            "worst-case {name} pairing block, validator re-execution: {ms:.0} ms (target on Raspberry Pi class hardware: < 2000 ms)"
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use revm::precompile::{PrecompileSpecId, Precompiles};

    /// The pairing vectors are valid: e(G1, G2) * e(-G1, G2) == 1, so the precompile does the
    /// full computation instead of failing fast on a bad point.
    #[test]
    fn pairing_inputs_are_valid_and_pair_to_one() {
        let p = Precompiles::new(PrecompileSpecId::OSAKA);
        for (addr, input) in [(8u8, bn254_pair_input(2)), (0x0f, bls_pair_input(2))] {
            let pairing = p.get(&Address::with_last_byte(addr)).expect("pairing precompile");
            let out = pairing.execute(&input, 1_000_000, 0).expect("valid input");
            assert_eq!(
                out.bytes.as_ref(),
                U256::from(1).to_be_bytes::<32>(),
                "precompile {addr:#x}"
            );
        }
    }
}
