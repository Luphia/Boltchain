//! Pre-block system calls (ADR 0006 §8), run after EIP-4788 / EIP-2935 and before transactions.

use crate::{abi::*, addresses::*, committee::Committee};
use alloy_consensus::Header;
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_sol_types::SolCall;
use bolt_exec::{BlockExecutor, block::BlockError};
use bolt_primitives::params::{
    BOOTSTRAP_EXIT_MIN_STAKERS, BOOTSTRAP_EXIT_MIN_TOTAL_STAKE_BOLT, CONSENSUS_REWARD_BPS,
    SUPPLY_CAP_WEI, WEI_PER_BOLT, epoch_emission,
};
use revm::DatabaseRef;

/// Epoch rules for the hooks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EpochRules {
    /// Blocks per epoch.
    pub epoch_slots: u64,
    /// Bootstrap exit: minimum number of stakers.
    pub exit_min_stakers: u64,
    /// Bootstrap exit: minimum total stake, in wei.
    pub exit_min_stake: U256,
}

impl EpochRules {
    /// Mainnet rules for `epoch_slots`.
    pub fn mainnet(epoch_slots: u64) -> Self {
        Self {
            epoch_slots,
            exit_min_stakers: BOOTSTRAP_EXIT_MIN_STAKERS as u64,
            exit_min_stake: U256::from(BOOTSTRAP_EXIT_MIN_TOTAL_STAKE_BOLT)
                * U256::from(WEI_PER_BOLT),
        }
    }

    /// Rules of a chain configuration (dev chains may lower the bootstrap exit thresholds).
    pub fn from_config(c: &bolt_primitives::genesis::ChainConfig) -> Self {
        let (stakers, stake) = c.bootstrap_exit();
        Self {
            epoch_slots: c.epoch_slots,
            exit_min_stakers: stakers as u64,
            exit_min_stake: U256::from(stake) * U256::from(WEI_PER_BOLT),
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

/// The votes certified by a block's parent certificate.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CertVotes {
    /// Epoch whose committee signed.
    pub epoch: u64,
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

/// Runs the Boltchain pre-block system calls for the block being built on `parent`.
pub fn pre_block<D: DatabaseRef>(
    exec: &mut BlockExecutor<D>,
    rules: &EpochRules,
    parent: &Header,
    votes: &CertVotes,
) -> Result<(), BlockError<D::Error>>
where
    D::Error: std::error::Error + Send + Sync + 'static,
{
    let number = exec.params().input.number;
    // 1. Burn accounting and participation.
    let burned = U256::from(parent.base_fee_per_gas.unwrap_or(0)) * U256::from(parent.gas_used);
    sys(
        exec,
        REWARDS,
        IRewardDistributor::onBlockCall {
            burned,
            certEpoch: votes.epoch,
            bitmap: Bytes::copy_from_slice(&votes.bitmap),
        },
    )?;
    if !rules.is_epoch_start(number) {
        return Ok(());
    }
    let epoch = rules.epoch_of(number);

    // 2. Rewards for the previous epoch.
    if epoch >= 1 {
        let supply = view(exec, REWARDS, IRewardDistributor::supplyCall {})?;
        let unissued = SUPPLY_CAP_WEI.saturating_sub(supply);
        let emission =
            epoch_emission(unissued) * U256::from(CONSENSUS_REWARD_BPS) / U256::from(10_000);
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

    // 3. Committee of the next epoch.
    let ended = view(exec, CONSENSUS, IConsensusRegistry::bootstrapEndedCall {})?;
    let mut end_now = false;
    if !ended {
        let stats = view(exec, STAKING, IStakingManager::stakerStatsCall {})?;
        end_now = stats.stakers >= U256::from(rules.exit_min_stakers)
            && stats.total >= rules.exit_min_stake;
    }
    let mut committee = if ended || end_now {
        let snap = view(exec, STAKING, IStakingManager::snapshotCall {})?;
        let mut pairs: Vec<(u32, U256)> = snap.ids.into_iter().zip(snap.stakes).collect();
        pairs.sort_by_key(|p| p.0);
        let (ids, stakes): (Vec<u32>, Vec<U256>) = pairs.into_iter().unzip();
        let size = view(exec, PARAMS, IParamRegistry::paramsCall {})?.committeeSize;
        Committee::sample(seed(parent), epoch + 1, &ids, &stakes, size)
    } else {
        Committee::equal(&view(exec, STAKING, IStakingManager::bootstrapSetCall {})?)
    };
    if committee.is_empty() {
        // Nobody eligible (everyone exited or was slashed): keep the current committee.
        let c = view(exec, CONSENSUS, IConsensusRegistry::committeeCall { epoch })?;
        committee = Committee::from_parts(c.ids, c.weights, &c.seats);
    }
    sys(
        exec,
        CONSENSUS,
        IConsensusRegistry::beginEpochCall {
            epoch,
            memberIds: committee.members.clone(),
            weights: committee.weights.clone(),
            seats: committee.seats_bytes(),
            endBootstrap: end_now,
        },
    )?;
    Ok(())
}

/// Committee lottery seed for the epoch after next: the RANDAO mix of the last block of the
/// previous epoch.
fn seed(parent: &Header) -> B256 {
    parent.mix_hash
}
