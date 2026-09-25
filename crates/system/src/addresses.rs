//! Fixed addresses of the system contracts (must match `contracts/src/System.sol`).

use alloy_primitives::{Address, address};

/// Caller of system calls (as EIP-4788 / EIP-2935).
pub const SYSTEM: Address = address!("0xfffffffffffffffffffffffffffffffffffffffe");

/// StakingManager proxy.
pub const STAKING: Address = address!("0xB017000000000000000000000000000000000001");
/// ConsensusRegistry proxy.
pub const CONSENSUS: Address = address!("0xB017000000000000000000000000000000000002");
/// RewardDistributor proxy.
pub const REWARDS: Address = address!("0xB017000000000000000000000000000000000003");
/// ParamRegistry proxy.
pub const PARAMS: Address = address!("0xB017000000000000000000000000000000000004");
/// HistoryRegistry proxy (M5).
pub const HISTORY: Address = address!("0xB017000000000000000000000000000000000005");

/// Governance multisig (SafeProxy).
pub const SAFE: Address = address!("0xB0170000000000000000000000000000000000A0");
/// 16-day timelock.
pub const UPGRADE_TIMELOCK: Address = address!("0xB0170000000000000000000000000000000000A1");
/// 2-day timelock.
pub const PARAM_TIMELOCK: Address = address!("0xB0170000000000000000000000000000000000A2");

/// Implementation behind each proxy, and the Safe singleton and fallback handler.
pub const STAKING_IMPL: Address = address!("0xB017000000000000000000000000000000000101");
/// ConsensusRegistry implementation.
pub const CONSENSUS_IMPL: Address = address!("0xB017000000000000000000000000000000000102");
/// RewardDistributor implementation.
pub const REWARDS_IMPL: Address = address!("0xB017000000000000000000000000000000000103");
/// ParamRegistry implementation.
pub const PARAMS_IMPL: Address = address!("0xB017000000000000000000000000000000000104");
/// HistoryRegistry implementation.
pub const HISTORY_IMPL: Address = address!("0xB017000000000000000000000000000000000105");
/// Safe 1.4.1 singleton.
pub const SAFE_SINGLETON: Address = address!("0xB0170000000000000000000000000000000001A0");
/// Safe CompatibilityFallbackHandler.
pub const SAFE_FALLBACK: Address = address!("0xB0170000000000000000000000000000000001A1");
