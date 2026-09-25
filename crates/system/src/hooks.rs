//! Pre-block system calls (ADR 0006 §8), run after EIP-4788 / EIP-2935 and before transactions.

use crate::{abi::*, addresses::*, committee::Committee};
use alloy_consensus::Header;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_sol_types::SolCall;
use bolt_exec::{BlockExecutor, block::BlockError};
use bolt_primitives::params::{
    CHECKPOINT_MINER_BPS, CONSENSUS_REWARD_BPS, SUPPLY_CAP_WEI, WEI_PER_BOLT, epoch_emission,
};
use revm::DatabaseRef;

/// Epoch and phase rules for the hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpochRules {
    /// Blocks per epoch.
    pub epoch_slots: u64,
    /// Committee seats.
    pub committee_size: u32,
    /// Stake-finality threshold: minimum number of stakers.
    pub checkpoint_min_stakers: u64,
    /// Stake-finality threshold: minimum total stake, in wei.
    pub checkpoint_min_stake: U256,
    /// Blocks that must bury a mined block before it is proposed as a checkpoint.
    pub checkpoint_depth: u64,
    /// PoS threshold: minimum number of stakers.
    pub pos_min_stakers: u64,
    /// PoS threshold: minimum total stake, in wei.
    pub pos_min_stake: U256,
    /// Consecutive epochs the thresholds must hold; minimum stake age for the first committee.
    pub pos_streak: u64,
}

impl EpochRules {
    /// Rules of a chain configuration (dev chains may lower the PoS thresholds).
    pub fn from_config(c: &bolt_primitives::genesis::ChainConfig) -> Self {
        let (stakers, stake, streak) = c.pos_thresholds();
        let (cp_stakers, cp_stake) = c.checkpoint_thresholds();
        Self {
            epoch_slots: c.epoch_slots,
            committee_size: c.committee_size,
            checkpoint_min_stakers: cp_stakers as u64,
            checkpoint_min_stake: U256::from(cp_stake) * U256::from(WEI_PER_BOLT),
            checkpoint_depth: c.checkpoint_depth(),
            pos_min_stakers: stakers as u64,
            pos_min_stake: U256::from(stake) * U256::from(WEI_PER_BOLT),
            pos_streak: streak,
        }
    }

    /// Epoch of block `height` (genesis and heights 1..=L are epoch 0).
    pub fn epoch_of(&self, height: u64) -> u64 {
        if height == 0 { 0 } else { (height - 1) / self.epoch_slots }
    }

    /// Whether `height` is the first block of its epoch.
    pub fn is_epoch_start(&self, height: u64) -> bool {
        height >= 1 && (height - 1).is_multiple_of(self.epoch_slots)
    }

    /// Last block of epoch `epoch`.
    pub fn epoch_end(&self, epoch: u64) -> u64 {
        (epoch + 1) * self.epoch_slots
    }
}

/// Consensus phase recorded in `ConsensusRegistry` (ADR 0007).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Phase {
    /// First epoch whose committee finalizes mined blocks (phase B), once scheduled.
    pub checkpoint_epoch: Option<u64>,
    /// First PoS epoch, once scheduled.
    pub pos_epoch: Option<u64>,
    /// Consecutive epochs the stake-finality thresholds have held.
    pub checkpoint_streak: u64,
    /// Consecutive epochs the PoS thresholds have held.
    pub streak: u64,
}

impl Phase {
    /// Whether mined block `height` belongs to an epoch whose committee finalizes checkpoints.
    pub fn is_checkpointing(&self, rules: &EpochRules, height: u64) -> bool {
        self.checkpoint_epoch.is_some_and(|e| rules.epoch_of(height) >= e)
            && !self.is_pos(rules, height)
    }

    /// Whether epoch `epoch` is a phase-B epoch (committee finalizes mined blocks).
    pub fn is_checkpoint_epoch(&self, epoch: u64) -> bool {
        self.checkpoint_epoch.is_some_and(|e| epoch >= e)
            && self.pos_epoch.is_none_or(|p| epoch < p)
    }

    /// Whether the first PoS epoch's anchor (the terminal block) was finalized by phase B, so the
    /// first PoS block carries its proof rather than nothing.
    pub fn terminal_is_checkpointed(&self) -> bool {
        match (self.checkpoint_epoch, self.pos_epoch) {
            (Some(c), Some(p)) => c < p,
            _ => false,
        }
    }

    /// Whether block `height` is produced by the PoS committee (otherwise it is mined).
    pub fn is_pos(&self, rules: &EpochRules, height: u64) -> bool {
        self.pos_epoch.is_some_and(|e| rules.epoch_of(height) >= e)
    }

    /// The last mined block (the PoS anchor), once PoS is scheduled after a mining phase.
    pub fn terminal_height(&self, rules: &EpochRules) -> Option<u64> {
        match self.pos_epoch {
            Some(0) | None => None,
            Some(e) => Some(rules.epoch_end(e - 1)),
        }
    }
}

/// The votes certified by a block's certificate.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CertVotes {
    /// Epoch whose committee signed.
    pub epoch: u64,
    /// Round of the certificate (each round's votes are counted once).
    pub round: u64,
    /// Signer bitmap over that committee's members.
    pub bitmap: Vec<u8>,
}

fn view<D: DatabaseRef, C: SolCall>(
    exec: &mut BlockExecutor<D>,
    to: Address,
    call: C,
) -> Result<C::Return, BlockError<D::Error>>
where
    D::Error: std::error::Error + Send + Sync + 'static,
{
    let out = exec.view_call(to, call.abi_encode().into())?;
    C::abi_decode_returns(&out).map_err(|e| BlockError::Evm(format!("decode {to}: {e}")))
}

fn sys<D: DatabaseRef, C: SolCall>(
    exec: &mut BlockExecutor<D>,
    to: Address,
    call: C,
) -> Result<C::Return, BlockError<D::Error>>
where
    D::Error: std::error::Error + Send + Sync + 'static,
{
    let out = exec.system_call_data(to, call.abi_encode().into())?;
    C::abi_decode_returns(&out).map_err(|e| BlockError::Evm(format!("decode {to}: {e}")))
}

/// How the block being executed is produced (for rewards).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Producer {
    /// Mined, phase A: the miner gets the whole block reward.
    Miner,
    /// Mined, phase B: the miner gets 60%; the committee earns the rest by participation.
    MinerWithCheckpoints,
    /// PoS committee.
    Committee,
}

/// Runs the Boltchain pre-block system calls for the block being built on `parent`.
pub fn pre_block<D: DatabaseRef>(
    exec: &mut BlockExecutor<D>,
    rules: &EpochRules,
    parent: &Header,
    votes: &CertVotes,
    producer: Producer,
) -> Result<(), BlockError<D::Error>>
where
    D::Error: std::error::Error + Send + Sync + 'static,
{
    let number = exec.params().input.number;
    // 1. Burn accounting, block reward, participation.
    let burned = U256::from(parent.base_fee_per_gas.unwrap_or(0)) * U256::from(parent.gas_used);
    let mut minted = U256::ZERO;
    if producer != Producer::Committee {
        let supply = view(exec, REWARDS, IRewardDistributor::supplyCall {})?;
        let unissued = SUPPLY_CAP_WEI.saturating_sub(supply);
        minted = bolt_pow::block_reward(unissued, CONSENSUS_REWARD_BPS);
        if producer == Producer::MinerWithCheckpoints {
            minted = minted * U256::from(CHECKPOINT_MINER_BPS) / U256::from(10_000);
        }
        if !minted.is_zero() {
            let beneficiary = exec.params().input.beneficiary;
            exec.credit(beneficiary, minted.to::<u128>())?;
        }
    }
    sys(
        exec,
        REWARDS,
        IRewardDistributor::onBlockCall {
            burned,
            minted,
            certEpoch: votes.epoch,
            certRound: votes.round,
            bitmap: Bytes::copy_from_slice(&votes.bitmap),
        },
    )?;
    if !rules.is_epoch_start(number) {
        return Ok(());
    }
    let epoch = rules.epoch_of(number);

    // 2. Rewards for the previous epoch's committee: none in phase A, the voters' 40% in phase B
    // (miners were paid per block), the whole consensus share under PoS.
    let before = crate::queries::phase_in(exec)?;
    if epoch >= 1 {
        let supply = view(exec, REWARDS, IRewardDistributor::supplyCall {})?;
        let unissued = SUPPLY_CAP_WEI.saturating_sub(supply);
        let mut emission =
            epoch_emission(unissued) * U256::from(CONSENSUS_REWARD_BPS) / U256::from(10_000);
        if before.is_checkpoint_epoch(epoch - 1) {
            emission = emission * U256::from(10_000 - CHECKPOINT_MINER_BPS) / U256::from(10_000);
        }
        let payout =
            view(exec, REWARDS, IRewardDistributor::previewCall { epoch: epoch - 1, emission })?;
        if !payout.is_zero() {
            exec.credit(REWARDS, payout.to::<u128>())?;
        }
        let paid =
            sys(exec, REWARDS, IRewardDistributor::settleCall { epoch: epoch - 1, emission })?;
        if paid != payout {
            return Err(BlockError::Evm(format!("settle paid {paid}, preview said {payout}")));
        }
    }

    // 3. Phase: count the epochs the thresholds have held (T1: checkpoints, T2: PoS).
    let stats = view(exec, STAKING, IStakingManager::stakerStatsCall {})?;
    let t1 = stats.stakers >= U256::from(rules.checkpoint_min_stakers)
        && stats.total >= rules.checkpoint_min_stake;
    let t2 =
        stats.stakers >= U256::from(rules.pos_min_stakers) && stats.total >= rules.pos_min_stake;
    let phase = sys(
        exec,
        CONSENSUS,
        IConsensusRegistry::beginEpochCall {
            epoch,
            checkpointMet: t1,
            posMet: t2,
            streakRequired: rules.pos_streak,
        },
    )?;

    // 4. Committee of the next epoch, from the first checkpoint epoch on. The first committee of
    // each phase (B and C) is drawn only from stake at least `pos_streak` epochs old (ADR 0007
    // §5), so nobody can buy a seat right before a phase starts.
    let next = epoch + 1;
    if !phase.cpScheduled || next < phase.firstCheckpointEpoch {
        return Ok(());
    }
    let first_of_phase =
        next == phase.firstCheckpointEpoch || (phase.scheduled && next == phase.firstPosEpoch);
    let min_age = if first_of_phase { rules.pos_streak } else { 0 };
    let mut committee = sample(exec, rules, parent, next, min_age)?;
    if committee.is_empty() && min_age > 0 {
        committee = sample(exec, rules, parent, next, 0)?;
    }
    if committee.is_empty() {
        // Nobody eligible (everyone exited or was slashed): keep the current committee.
        let c = view(exec, CONSENSUS, IConsensusRegistry::committeeCall { epoch })?;
        committee = Committee::from_parts(c.ids, c.weights, &c.seats);
    }
    sys(
        exec,
        CONSENSUS,
        IConsensusRegistry::setCommitteeCall {
            epoch: next,
            memberIds: committee.members.clone(),
            weights: committee.weights.clone(),
            seats: committee.seats_bytes(),
        },
    )?;
    Ok(())
}

fn sample<D: DatabaseRef>(
    exec: &mut BlockExecutor<D>,
    rules: &EpochRules,
    parent: &Header,
    epoch: u64,
    min_age: u64,
) -> Result<Committee, BlockError<D::Error>>
where
    D::Error: std::error::Error + Send + Sync + 'static,
{
    let snap = view(exec, STAKING, IStakingManager::snapshotCall { minAge: min_age })?;
    let mut pairs: Vec<(u32, U256)> = snap.ids.into_iter().zip(snap.stakes).collect();
    pairs.sort_by_key(|p| p.0);
    let (ids, stakes): (Vec<u32>, Vec<U256>) = pairs.into_iter().unzip();
    Ok(Committee::sample(seed(parent), epoch, &ids, &stakes, rules.committee_size))
}

/// Committee lottery seed for the epoch after next: the RANDAO mix of the last block of the
/// previous epoch.
fn seed(parent: &Header) -> B256 {
    parent.mix_hash
}
