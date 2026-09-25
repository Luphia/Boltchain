//! ABI bindings for the calls the node makes (and tests use).

#![allow(missing_docs)]

use alloy_sol_types::sol;

sol! {
    interface IStakingManager {
        enum Status { None, Active, Exiting, Slashed, Withdrawn }

        function registerBootstrap(bytes pubkey, address feeRecipient) external returns (uint32 id);
        function register(bytes pubkey, bytes pubkeyPoint, bytes pop, address feeRecipient)
            external payable returns (uint32 id);
        function deposit(uint32 id) external payable;
        function requestExit(uint32 id) external;
        function withdraw(uint32 id) external;
        function setFeeRecipient(uint32 id, address feeRecipient) external;
        function setDepositsPaused(bool paused) external;

        function snapshot() external view returns (uint32[] ids, uint256[] stakes);
        function bootstrapSet() external view returns (uint32[] ids);
        function stakerStats() external view returns (uint256 stakers, uint256 total);
        function keysOf(uint32[] ids) external view returns (bytes pubkeys, address[] recipients);
        function pubkeyOf(uint32 id) external view returns (bytes);
        function totalActiveStake() external view returns (uint256);
        function weightOf(uint32 id) external view returns (uint256);
        function deadStake() external view returns (uint256);
        function count() external view returns (uint32);
        function idOfPubkey(bytes32 h) external view returns (uint32);
        function validator(uint32 id) external view returns (
            address owner, address feeRecipient, uint256 stake, uint256 locked,
            Status status, bool bootstrap, uint64 exitEpoch, bytes pubkey);
    }

    interface IConsensusRegistry {
        function initialize(uint32[] memberIds, uint16[] weights, bytes seats) external;
        function beginEpoch(uint64 epoch, uint32[] memberIds, uint16[] weights, bytes seats, bool endBootstrap)
            external;
        function currentEpoch() external view returns (uint64);
        function bootstrapEnded() external view returns (bool);
        function bootstrapEndEpoch() external view returns (uint64);
        function committee(uint64 epoch) external view returns (uint32[] ids, uint16[] weights, bytes seats);
        function submitEvidence(uint32 id, bytes pubkeyPoint, bytes msgA, bytes sigA, bytes msgB, bytes sigB)
            external returns (uint256 slashed);
        function slashBps() external view returns (uint256);
    }

    interface IRewardDistributor {
        function initialize(uint256 genesisSupply) external;
        function onBlock(uint256 burned, uint64 certEpoch, bytes bitmap) external;
        function settle(uint64 epoch, uint256 emission) external returns (uint256 paid);
        function preview(uint64 epoch, uint256 emission) external view returns (uint256 paid);
        function claim(uint32 id) external returns (uint256 amount);
        function supply() external view returns (uint256);
        function rewards(uint32 id) external view returns (uint256);
        function votesOf(uint64 epoch, uint256 index) external view returns (uint256);
    }

    interface IParamRegistry {
        function initialize(uint64 gasLimit, uint64 minBaseFee, uint32 committeeSize, uint32 floor, uint128 lockedWeightCap) external;
        function lockedWeightCap() external view returns (uint128);
        function setLockedWeightCap(uint128 v) external;
        function params() external view returns (uint64 gasLimit, uint64 minBaseFee, uint32 committeeSize);
        function setGasLimit(uint64 v) external;
        function setCommitteeSize(uint32 v) external;
    }

    interface ISafe {
        function setup(
            address[] owners, uint256 threshold, address to, bytes data,
            address fallbackHandler, address paymentToken, uint256 payment, address paymentReceiver
        ) external;
        function getOwners() external view returns (address[]);
        function getThreshold() external view returns (uint256);
    }

    interface ITimelock {
        function getMinDelay() external view returns (uint256);
        function hasRole(bytes32 role, address account) external view returns (bool);
        function schedule(address target, uint256 value, bytes data, bytes32 predecessor, bytes32 salt, uint256 delay)
            external;
    }

    interface IUpgradeable {
        function upgradeToAndCall(address newImplementation, bytes data) external payable;
    }

    interface IBLSHarness {
        function hashToG2(bytes message, bytes dst) external view returns (bytes);
        function expand(bytes message, bytes dst) external pure returns (bytes);
        function verify(bytes pk, bytes message, bytes sig) external view returns (bool);
        function verifyPop(bytes pk, bytes pubkey, bytes pop) external view returns (bool);
        function compress(bytes pk) external pure returns (bytes);
    }
}
