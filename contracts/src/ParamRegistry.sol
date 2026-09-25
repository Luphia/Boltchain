// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {Sys, SystemContract} from "./System.sol";

/// @title On-chain protocol parameters. The node reads them at each epoch start; changes take
/// effect from the next epoch.
/// @notice Hard bounds are in code: governance can move a parameter only within its range, and the
/// committee size can never go below the floor fixed at genesis (512 on mainnet).
contract ParamRegistry is SystemContract {
    uint64 public constant GAS_LIMIT_MIN = 15_000_000;
    uint64 public constant GAS_LIMIT_MAX = 60_000_000;
    uint64 public constant MIN_BASE_FEE_MIN = 1_000_000; // 0.001 gwei
    uint64 public constant MIN_BASE_FEE_MAX = 10_000_000_000; // 10 gwei
    uint32 public constant COMMITTEE_MAX = 4096;

    uint64 public gasLimit;
    uint64 public minBaseFee;
    uint32 public committeeSize;
    uint32 public committeeFloor;
    /// Most of a validator's locked bootstrap-phase rewards that counts as stake in the committee
    /// lottery and the bootstrap exit condition (ADR 0006 §11).
    uint128 public lockedWeightCap;

    event ParamChanged(bytes32 indexed name, uint256 value);

    error OutOfRange();

    /// Genesis values. Dev chains may use a floor below 512; mainnet genesis validation forbids it.
    function initialize(
        uint64 gasLimit_,
        uint64 minBaseFee_,
        uint32 committeeSize_,
        uint32 floor_,
        uint128 lockedWeightCap_
    ) external onlyGenesis {
        committeeFloor = floor_;
        lockedWeightCap = lockedWeightCap_;
        gasLimit = gasLimit_;
        minBaseFee = minBaseFee_;
        committeeSize = committeeSize_;
    }

    /// Bounded adjustment (2-day timelock, or the 16-day one).
    function setGasLimit(uint64 v) external {
        _onlyTimelock();
        if (v < GAS_LIMIT_MIN || v > GAS_LIMIT_MAX) revert OutOfRange();
        gasLimit = v;
        emit ParamChanged("gasLimit", v);
    }

    /// Bounded adjustment (2-day timelock, or the 16-day one).
    function setMinBaseFee(uint64 v) external {
        _onlyTimelock();
        if (v < MIN_BASE_FEE_MIN || v > MIN_BASE_FEE_MAX) revert OutOfRange();
        minBaseFee = v;
        emit ParamChanged("minBaseFee", v);
    }

    /// Security-relevant: 16-day timelock only; never below the genesis floor.
    function setCommitteeSize(uint32 v) external {
        if (msg.sender != Sys.UPGRADE_TIMELOCK) revert Unauthorized();
        if (v < committeeFloor || v > COMMITTEE_MAX) revert OutOfRange();
        committeeSize = v;
        emit ParamChanged("committeeSize", v);
    }

    /// Security-relevant: 16-day timelock only; at least the minimum stake.
    function setLockedWeightCap(uint128 v) external {
        if (msg.sender != Sys.UPGRADE_TIMELOCK) revert Unauthorized();
        if (v < Sys.MIN_STAKE) revert OutOfRange();
        lockedWeightCap = v;
        emit ParamChanged("lockedWeightCap", v);
    }

    /// All parameters in one call, for the node.
    function params() external view returns (uint64, uint64, uint32) {
        return (gasLimit, minBaseFee, committeeSize);
    }

    function _onlyTimelock() private view {
        if (msg.sender != Sys.PARAM_TIMELOCK && msg.sender != Sys.UPGRADE_TIMELOCK) revert Unauthorized();
    }
}
