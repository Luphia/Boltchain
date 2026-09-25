//! Protocol constants for Boltchain.
//!
//! Values that governance may tune live in `ParamRegistry` on chain; the constants here are
//! either immutable protocol rules or the genesis defaults for those tunable parameters.

use alloy_primitives::U256;

/// EIP-155 chain id. Boltchain is the successor chain of iSunCoin Mainnet and keeps its id.
pub const CHAIN_ID: u64 = 8017;

/// Slot length in seconds.
pub const SLOT_SECONDS: u64 = 6;

/// Slots per epoch (one day).
pub const EPOCH_SLOTS: u64 = 14_400;

/// Committee seats sampled per epoch. This is a hard floor: governance may raise it, never lower it.
///
/// With an adversary holding at most 20% of stake, the chance of sampling more than 1/3 of
/// 512 seats is about 8.5e-13 per epoch.
pub const MIN_COMMITTEE_SIZE: u32 = 512;

/// Default block gas limit.
pub const DEFAULT_GAS_LIMIT: u64 = 30_000_000;

/// Allowed range for governance adjustments of the block gas limit.
pub const GAS_LIMIT_RANGE: (u64, u64) = (15_000_000, 60_000_000);

/// EIP-7934: maximum RLP-encoded block size (10 MiB minus a 2 MiB safety margin).
pub const MAX_RLP_BLOCK_SIZE: usize = 10 * 1024 * 1024 - 2 * 1024 * 1024;

/// Minimum base fee: 0.01 gwei.
pub const MIN_BASE_FEE_WEI: u64 = 10_000_000;

/// Unbonding period in epochs (14 days). Also the weak-subjectivity window.
pub const UNBONDING_EPOCHS: u64 = 14;

/// Epochs of history every validator must keep pinned.
pub const RECENT_PINNED_EPOCHS: u64 = 7;

/// Replication factor for older history shards.
pub const HISTORY_REPLICATION: u32 = 16;

/// Storage audits drawn per epoch (ADR 0009).
pub const AUDIT_TASKS: u32 = 16;

/// Auditors per epoch, drawn from the committee.
pub const AUDIT_PANEL: u32 = 16;

/// Share of the stake slashed when an audit panel finds a provider's shard unavailable, in
/// basis points.
pub const AUDIT_SLASH_BPS: u32 = 100;

/// Maximum size of a single IPFS block (bitswap compatible).
pub const MAX_IPLD_BLOCK_BYTES: usize = 1 << 20;

/// Bodies smaller than this are inlined into the gossipsub announcement.
pub const INLINE_BODY_BYTES: usize = 64 * 1024;

/// One BOLT in wei.
pub const WEI_PER_BOLT: u128 = 1_000_000_000_000_000_000;

/// Total supply cap in whole BOLT: 2^32.
pub const SUPPLY_CAP_BOLT: u64 = 1 << 32;

/// Total supply cap in wei.
pub const SUPPLY_CAP_WEI: U256 = U256::from_limbs([0xa764_0000_0000_0000, 0x0de0_b6b3, 0, 0]);

/// Minimum stake: 64 BOLT, in wei.
pub const MIN_STAKE_WEI: u128 = 64 * WEI_PER_BOLT;

/// Emission half-life, in epochs (4 years of 365 days).
pub const EMISSION_HALF_LIFE_EPOCHS: u64 = 4 * 365;

/// Per-epoch emission rate `k = 1 - 2^(-1/1460)`, scaled by 1e18.
pub const EMISSION_RATE_E18: u128 = 474_645_662_939_840;

/// Share of emission paid to consensus participation, in basis points. The rest goes to history
/// storage ([`STORAGE_REWARD_BPS`]).
pub const CONSENSUS_REWARD_BPS: u32 = 8_000;

/// Share of emission paid to history storage providers that pass audits, in basis points.
pub const STORAGE_REWARD_BPS: u32 = 10_000 - CONSENSUS_REWARD_BPS;

/// Stake finality start (phase B, ADR 0007 §4): minimum number of stakers.
pub const CHECKPOINT_MIN_STAKERS: u32 = 32;

/// Stake finality start: minimum total stake, in whole BOLT.
pub const CHECKPOINT_MIN_TOTAL_STAKE_BOLT: u64 = 1_000_000;

/// Phase B: a mined block is proposed as a checkpoint once this many blocks bury it.
pub const CHECKPOINT_DEPTH: u64 = 32;

/// Phase B: miners' share of each block reward, in basis points; the rest pays the committee
/// by participation.
pub const CHECKPOINT_MINER_BPS: u32 = 6_000;

/// PoS start (phase C, ADR 0007): minimum number of stakers with at least the minimum stake.
pub const POS_MIN_STAKERS: u32 = 128;

/// PoS start: minimum total stake, in whole BOLT.
pub const POS_MIN_TOTAL_STAKE_BOLT: u64 = 10_000_000;

/// PoS start: consecutive epochs the thresholds must hold; also the minimum age of the stake the
/// first PoS committee is drawn from.
pub const POS_STREAK_EPOCHS: u64 = 14;

/// Target spacing of mined blocks, in seconds.
pub const POW_BLOCK_SECONDS: u64 = 12;

/// ASERT half-life, in seconds.
pub const POW_HALF_LIFE_SECONDS: u64 = 3_600;

/// Difficulty of the first mined blocks (about 22,000 RandomBOLT hashes per second at 12 s).
pub const POW_INITIAL_DIFFICULTY: u64 = 1 << 18;

/// Deepest reorganisation a node follows automatically (equals the retained state history).
pub const MAX_REORG_DEPTH: u64 = 128;

// Compile-time sanity checks on the constants above.
const _: () = assert!(MIN_COMMITTEE_SIZE >= 512);
const _: () =
    assert!(GAS_LIMIT_RANGE.0 <= DEFAULT_GAS_LIMIT && DEFAULT_GAS_LIMIT <= GAS_LIMIT_RANGE.1);

/// Emission for one epoch, given the current unissued pool (cap minus circulating supply).
///
/// Burned base fees flow back into the unissued pool, so emission never reaches zero and the
/// circulating supply never exceeds [`SUPPLY_CAP_WEI`].
pub fn epoch_emission(unissued: U256) -> U256 {
    unissued * U256::from(EMISSION_RATE_E18) / U256::from(WEI_PER_BOLT)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn supply_cap_is_two_pow_32_bolt() {
        let expected = U256::from(SUPPLY_CAP_BOLT) * U256::from(WEI_PER_BOLT);
        assert_eq!(SUPPLY_CAP_WEI, expected);
    }

    #[test]
    fn emission_halves_after_half_life() {
        let mut unissued = SUPPLY_CAP_WEI;
        for _ in 0..EMISSION_HALF_LIFE_EPOCHS {
            unissued -= epoch_emission(unissued);
        }
        let half = SUPPLY_CAP_WEI / U256::from(2);
        let diff = if unissued > half { unissued - half } else { half - unissued };
        // within 1 ppm of exactly half
        assert!(diff < SUPPLY_CAP_WEI / U256::from(1_000_000u64), "diff = {diff}");
    }

    #[test]
    fn first_epoch_emission_matches_plan() {
        let bolt = epoch_emission(SUPPLY_CAP_WEI) / U256::from(WEI_PER_BOLT);
        // 2,038,587.x BOLT (the plan rounds it to 2,038,588)
        assert_eq!(bolt, U256::from(2_038_587u64));
    }
}
