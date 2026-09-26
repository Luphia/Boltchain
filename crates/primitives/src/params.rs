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

/// Additional copies of each old epoch kept by storage providers without stake (ADR 0012), on
/// top of the [`HISTORY_REPLICATION`] validators.
pub const STORAGE_REPLICATION: u32 = 16;

/// First id of a storage provider without stake (`HistoryRegistry.STORAGE_ID_BASE`).
pub const STORAGE_ID_BASE: u32 = 1 << 31;

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

/// Minimum stake: 64 BOLT, in wei.
pub const MIN_STAKE_WEI: u128 = 64 * WEI_PER_BOLT;

/// Whole emission of a block in the first era, in whole BOLT (ADR 0012): the consensus reward
/// plus the storage reward. There is no supply cap.
pub const INITIAL_BLOCK_REWARD_BOLT: u64 = 32;

/// Storage reward of every block, in whole BOLT, forever: paid per epoch to the history audits
/// passed in it (ADR 0009, 0012).
pub const STORAGE_BLOCK_REWARD_BOLT: u64 = 1;

/// Lowest consensus reward of a block, in whole BOLT, reached after four halvings and paid forever.
pub const TAIL_CONSENSUS_REWARD_BOLT: u64 = 1;

/// Length of an emission era: the block reward halves every 4 years of 365 days of chain time
/// (block timestamps since genesis), whatever the block spacing (12 s mined, 6 s PoS slots).
pub const HALVING_SECONDS: u64 = 4 * 365 * 86_400;

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

/// Emission era of a block with timestamp `timestamp` on a chain started at `genesis_timestamp`.
pub fn emission_era(genesis_timestamp: u64, timestamp: u64) -> u64 {
    timestamp.saturating_sub(genesis_timestamp) / HALVING_SECONDS
}

/// Consensus reward of one block in era `era`, in wei: 31, 15, 7, 3, then 1 BOLT forever (the
/// block's 32, 16, 8, 4, 2 BOLT minus the storage reward, never below the tail).
pub fn consensus_reward(era: u64) -> U256 {
    let total = INITIAL_BLOCK_REWARD_BOLT.checked_shr(era.min(63) as u32).unwrap_or(0);
    let bolt = total.saturating_sub(STORAGE_BLOCK_REWARD_BOLT).max(TAIL_CONSENSUS_REWARD_BOLT);
    U256::from(bolt) * U256::from(WEI_PER_BOLT)
}

/// Storage reward of one block, in wei (1 BOLT in every era).
pub fn storage_reward() -> U256 {
    U256::from(STORAGE_BLOCK_REWARD_BOLT) * U256::from(WEI_PER_BOLT)
}

/// `bps` basis points of `amount`.
pub fn share(amount: U256, bps: u32) -> U256 {
    amount * U256::from(bps) / U256::from(10_000)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_reward_halves_every_four_years_down_to_the_tail() {
        let bolt = |w: U256| (w / U256::from(WEI_PER_BOLT)).to::<u64>();
        let consensus: Vec<u64> = (0..8).map(|e| bolt(consensus_reward(e))).collect();
        assert_eq!(consensus, [31, 15, 7, 3, 1, 1, 1, 1]);
        let total: Vec<u64> =
            (0..8).map(|e| bolt(consensus_reward(e) + storage_reward())).collect();
        assert_eq!(total, [32, 16, 8, 4, 2, 2, 2, 2]);
        assert_eq!(bolt(consensus_reward(u64::MAX)), 1, "the tail is paid forever");
    }

    #[test]
    fn eras_follow_chain_time() {
        let g = 1_800_000_000;
        assert_eq!(emission_era(g, g), 0);
        assert_eq!(emission_era(g, g + HALVING_SECONDS - 1), 0);
        assert_eq!(emission_era(g, g + HALVING_SECONDS), 1);
        assert_eq!(emission_era(g, g + 5 * HALVING_SECONDS + 7), 5);
        assert_eq!(emission_era(g, g - 10), 0, "clock skew before genesis stays in era 0");
    }
}
