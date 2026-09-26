//! Fixed addresses of the system contracts (must match `contracts/src/System.sol`).

use alloy_primitives::{Address, address};

/// Caller of system calls (as EIP-4788 / EIP-2935).
pub const SYSTEM: Address = address!("0xfffffffffffffffffffffffffffffffffffffffe");

/// StakingManager.
pub const STAKING: Address = address!("0xB017000000000000000000000000000000000001");
/// ConsensusRegistry.
pub const CONSENSUS: Address = address!("0xB017000000000000000000000000000000000002");
/// RewardDistributor.
pub const REWARDS: Address = address!("0xB017000000000000000000000000000000000003");
/// SwarmStorage (M5).
pub const SWARM: Address = address!("0xB017000000000000000000000000000000000005");
/// ComputeMarket (ADR 0011).
pub const COMPUTE: Address = address!("0xB017000000000000000000000000000000000006");
