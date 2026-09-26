//! Boltchain proof of work (ADR 0007 §1).
//!
//! * **RandomBOLT**: RandomX with Boltchain's Argon2 salt (`third_party/randomx`), so Monero
//!   hashrate cannot be reused. Nodes verify in light mode (256 MiB cache, ~30 ms per hash on a
//!   desktop CPU); miners may use fast mode (2 GiB dataset).
//! * **Seal**: `pow = RandomBOLT(key, seal_hash ‖ nonce)` where `seal_hash` is the header hash
//!   with the nonce zeroed; valid if `pow ≤ 2²⁵⁶ / difficulty`.
//! * **Key rotation**: the RandomX key of block `h` is the hash of block
//!   [`seed_height`]`(h)`, which changes every [`SEED_EPOCH`] blocks with a [`SEED_LAG`] delay.
//! * **Difficulty**: relative ASERT, see [`next_difficulty`].

mod asert;
mod randombolt;

pub use asert::{AsertParams, next_difficulty};
pub use randombolt::{Hasher, Mode, PowError};

use alloy_consensus::Header;
use alloy_primitives::{B64, B256, U256, keccak256};

/// Blocks between RandomX key changes.
pub const SEED_EPOCH: u64 = 2048;
/// Delay before a new key takes effect (so miners can prepare the dataset in advance).
pub const SEED_LAG: u64 = 64;

/// Height of the block whose hash keys RandomBOLT for block `height` (0 = genesis).
pub fn seed_height(height: u64) -> u64 {
    if height <= SEED_EPOCH + SEED_LAG {
        0
    } else {
        (height - SEED_LAG - 1) / SEED_EPOCH * SEED_EPOCH
    }
}

/// The header hash the miner commits to: everything but the nonce.
pub fn seal_hash(header: &Header) -> B256 {
    let mut h = header.clone();
    h.nonce = B64::ZERO;
    h.hash_slow()
}

/// RandomBOLT input: `seal_hash ‖ nonce` (40 bytes).
pub fn pow_input(seal: &B256, nonce: u64) -> [u8; 40] {
    let mut buf = [0u8; 40];
    buf[..32].copy_from_slice(seal.as_slice());
    buf[32..].copy_from_slice(&nonce.to_be_bytes());
    buf
}

/// Whether `pow` meets `difficulty` (`pow ≤ ⌊(2²⁵⁶ − 1) / difficulty⌋`).
pub fn meets(pow: &B256, difficulty: U256) -> bool {
    if difficulty.is_zero() {
        return false;
    }
    U256::from_be_bytes(pow.0) <= U256::MAX / difficulty
}

/// Work represented by a block of `difficulty` (the expected number of hashes), for total
/// difficulty accounting.
pub fn work(difficulty: U256) -> U256 {
    difficulty
}

/// Proof-of-work function of a chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    /// RandomBOLT (every public network).
    RandomBolt,
    /// keccak256(key ‖ input): a fast stand-in for tests on dev chains.
    Keccak,
}

/// Hashes seals with a chain's algorithm.
#[derive(Debug, Clone)]
pub struct Pow {
    algorithm: Algorithm,
    hasher: Option<std::sync::Arc<Hasher>>,
}

impl Pow {
    /// Verification: RandomBOLT in light mode, one hasher shared by the whole process (each
    /// cached key costs 256 MiB).
    pub fn light(algorithm: Algorithm) -> Self {
        static LIGHT: std::sync::OnceLock<std::sync::Arc<Hasher>> = std::sync::OnceLock::new();
        let hasher = (algorithm == Algorithm::RandomBolt)
            .then(|| LIGHT.get_or_init(|| std::sync::Arc::new(Hasher::new(Mode::Light))).clone());
        Self { algorithm, hasher }
    }

    /// Mining: RandomBOLT in `mode` with its own hasher (fast mode allocates 2 GiB per key).
    pub fn with_mode(algorithm: Algorithm, mode: Mode) -> Self {
        if mode == Mode::Light {
            return Self::light(algorithm);
        }
        let hasher =
            (algorithm == Algorithm::RandomBolt).then(|| std::sync::Arc::new(Hasher::new(mode)));
        Self { algorithm, hasher }
    }

    /// Algorithm.
    pub fn algorithm(&self) -> Algorithm {
        self.algorithm
    }

    fn hash_batch(&self, key: &B256, inputs: &[&[u8]]) -> Result<Vec<B256>, PowError> {
        match &self.hasher {
            Some(h) => h.hash_batch(key, inputs),
            None => Ok(inputs.iter().map(|i| keccak256([key.as_slice(), i].concat())).collect()),
        }
    }

    /// The PoW hash of `seal` with `nonce`.
    pub fn hash(&self, key: &B256, seal: &B256, nonce: u64) -> Result<B256, PowError> {
        let input = pow_input(seal, nonce);
        Ok(self.hash_batch(key, &[input.as_slice()])?.remove(0))
    }

    /// Whether `header`'s nonce seals it at its own difficulty under `key`.
    pub fn verify(&self, key: &B256, header: &Header) -> Result<bool, PowError> {
        let pow = self.hash(key, &seal_hash(header), u64::from_be_bytes(header.nonce.0))?;
        Ok(meets(&pow, header.difficulty))
    }

    /// Searches nonces `start, start + step, …` (up to `tries` of them) for one whose hash meets
    /// `difficulty`. Checks `stop` between batches. Returns (nonce, pow hash).
    #[allow(clippy::too_many_arguments)]
    pub fn search(
        &self,
        key: &B256,
        seal: &B256,
        difficulty: U256,
        start: u64,
        step: u64,
        tries: u64,
        stop: &std::sync::atomic::AtomicBool,
    ) -> Result<Option<(u64, B256)>, PowError> {
        const BATCH: u64 = 16;
        let mut n = start;
        let mut done = 0;
        while done < tries {
            if stop.load(std::sync::atomic::Ordering::Relaxed) {
                return Ok(None);
            }
            let count = BATCH.min(tries - done);
            let inputs: Vec<[u8; 40]> =
                (0..count).map(|i| pow_input(seal, n.wrapping_add(i * step))).collect();
            let refs: Vec<&[u8]> = inputs.iter().map(|i| i.as_slice()).collect();
            for (i, pow) in self.hash_batch(key, &refs)?.into_iter().enumerate() {
                if meets(&pow, difficulty) {
                    return Ok(Some((n.wrapping_add(i as u64 * step), pow)));
                }
            }
            n = n.wrapping_add(count * step);
            done += count;
        }
        Ok(None)
    }
}

/// Keccak of the RandomX key material for `seed_block` (domain-separated).
pub fn key_for(seed_block: &B256) -> B256 {
    keccak256([b"boltchain/pow-key/v1".as_slice(), seed_block.as_slice()].concat())
}

#[cfg(test)]
mod tests;
