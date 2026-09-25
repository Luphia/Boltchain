// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {Sys, SystemContract} from "./System.sol";
import {BLS} from "./BLS.sol";

interface IConsensusRegistry {
    function currentEpoch() external view returns (uint64);
    function bootstrapEnded() external view returns (bool);
}

interface IRewardDistributor {
    function burnSupply(uint256 amount) external;
}

/// @title Validator registry and stake custody.
/// @notice One BLS key per validator, registered with an on-chain verified proof of possession.
/// No delegation (liquid staking can be built on top). Exits take effect after 14 epochs.
contract StakingManager is SystemContract {
    enum Status {
        None,
        Active,
        Exiting,
        Slashed,
        Withdrawn
    }

    struct Validator {
        address owner; // controls the stake; receives it on withdrawal
        address feeRecipient; // receives rewards and block tips
        uint128 stake; // wei
        uint128 locked; // part of `stake` locked until the bootstrap phase ends
        Status status;
        bool bootstrap; // listed in genesis
        uint64 exitEpoch; // epoch in which the exit was requested (or the slashing happened)
        bytes32 pk0; // compressed BLS public key, bytes 0..32
        bytes16 pk1; // bytes 32..48
    }

    uint32 public count;
    mapping(uint32 => Validator) internal validators;
    mapping(bytes32 => uint32) public idOfPubkey; // keccak(compressed pubkey) => id
    uint32[] internal active; // Active validators (any stake)
    mapping(uint32 => uint32) internal activePos; // id => index + 1 in `active`
    uint32[] internal bootstrapIds;
    uint256 public totalActiveStake;
    uint256 public deadStake; // slashed funds kept locked forever (burned)
    bool public depositsPaused;

    event Registered(uint32 indexed id, address indexed owner, bytes pubkey, address feeRecipient, uint256 stake);
    event Deposited(uint32 indexed id, uint256 amount);
    event ExitRequested(uint32 indexed id, uint64 epoch);
    event Withdrawn(uint32 indexed id, uint256 amount);
    event Slashed(uint32 indexed id, uint256 amount, address reporter, uint256 reward);
    event FeeRecipientChanged(uint32 indexed id, address feeRecipient);
    event DepositsPaused(bool paused);

    error Paused();
    error BelowMinimum();
    error BadKey();
    error KeyTaken();
    error BadProofOfPossession();
    error NotOwner();
    error BadStatus();
    error StillBonded();
    error Locked();
    error TransferFailed();

    // ---------------------------------------------------------------- genesis

    /// Registers a genesis bootstrap validator (no stake; its PoP and address binding were checked
    /// by the node when validating genesis).
    function registerBootstrap(bytes calldata pubkey, address feeRecipient) external onlyGenesis returns (uint32 id) {
        id = _create(pubkey, feeRecipient, feeRecipient, 0);
        validators[id].bootstrap = true;
        bootstrapIds.push(id);
    }

    // ---------------------------------------------------------------- validators

    /// Registers a new validator with at least 64 BOLT.
    /// @param pubkey compressed BLS public key (48 bytes)
    /// @param pubkeyPoint the same key as an EIP-2537 G1 point (128 bytes)
    /// @param pop proof of possession as an EIP-2537 G2 point (256 bytes)
    function register(bytes calldata pubkey, bytes calldata pubkeyPoint, bytes calldata pop, address feeRecipient)
        external
        payable
        returns (uint32 id)
    {
        if (depositsPaused) revert Paused();
        if (msg.value < Sys.MIN_STAKE) revert BelowMinimum();
        if (pubkey.length != 48 || keccak256(BLS.compressG1(pubkeyPoint)) != keccak256(pubkey)) revert BadKey();
        if (!BLS.verify(pubkeyPoint, pubkey, pop, BLS.POP_DST)) revert BadProofOfPossession();
        id = _create(pubkey, msg.sender, feeRecipient, msg.value);
        totalActiveStake += msg.value;
    }

    /// Adds stake to an active validator (anyone may top up).
    function deposit(uint32 id) external payable {
        if (depositsPaused) revert Paused();
        Validator storage v = validators[id];
        if (v.status != Status.Active) revert BadStatus();
        v.stake += uint128(msg.value);
        totalActiveStake += msg.value;
        emit Deposited(id, msg.value);
    }

    /// Leaves the validator set. The validator is not sampled for committees chosen from now on
    /// and can withdraw 14 epochs after the next epoch.
    function requestExit(uint32 id) external {
        Validator storage v = validators[id];
        if (msg.sender != v.owner) revert NotOwner();
        if (v.status != Status.Active) revert BadStatus();
        v.status = Status.Exiting;
        v.exitEpoch = _epoch();
        _deactivate(id, v.stake);
        emit ExitRequested(id, v.exitEpoch);
    }

    /// Withdraws the whole remaining stake after the unbonding period.
    function withdraw(uint32 id) external {
        Validator storage v = validators[id];
        if (msg.sender != v.owner) revert NotOwner();
        if (v.status != Status.Exiting && v.status != Status.Slashed) revert BadStatus();
        if (_epoch() < v.exitEpoch + 1 + Sys.UNBONDING_EPOCHS) revert StillBonded();
        if (v.locked > 0 && !IConsensusRegistry(Sys.CONSENSUS).bootstrapEnded()) revert Locked();
        uint256 amount = v.stake;
        v.stake = 0;
        v.locked = 0;
        v.status = Status.Withdrawn;
        emit Withdrawn(id, amount);
        (bool ok,) = v.owner.call{value: amount}("");
        if (!ok) revert TransferFailed();
    }

    function setFeeRecipient(uint32 id, address feeRecipient) external {
        Validator storage v = validators[id];
        if (msg.sender != v.owner) revert NotOwner();
        v.feeRecipient = feeRecipient;
        emit FeeRecipientChanged(id, feeRecipient);
    }

    // ---------------------------------------------------------------- system

    /// Slashes `bps` basis points of the stake and forces an exit. 1% of the slashed amount goes to
    /// the reporter; the rest stays locked here forever and leaves the circulating supply.
    function slash(uint32 id, uint256 bps, address reporter) external returns (uint256 amount) {
        if (msg.sender != Sys.CONSENSUS) revert Unauthorized();
        Validator storage v = validators[id];
        if (v.status != Status.Active && v.status != Status.Exiting) revert BadStatus();
        amount = uint256(v.stake) * bps / 10_000;
        uint256 reward = amount / 100;
        if (v.status == Status.Active) {
            _deactivate(id, v.stake);
        }
        v.stake -= uint128(amount);
        v.locked = v.locked > v.stake ? v.stake : v.locked;
        v.status = Status.Slashed;
        v.exitEpoch = _epoch();
        deadStake += amount - reward;
        IRewardDistributor(Sys.REWARDS).burnSupply(amount - reward);
        emit Slashed(id, amount, reporter, reward);
        if (reward > 0) {
            (bool ok,) = reporter.call{value: reward}("");
            if (!ok) revert TransferFailed();
        }
    }

    /// Adds bootstrap-phase rewards to a validator's stake, locked until the phase ends.
    function lockReward(uint32 id) external payable {
        if (msg.sender != Sys.REWARDS) revert Unauthorized();
        Validator storage v = validators[id];
        v.stake += uint128(msg.value);
        v.locked += uint128(msg.value);
        if (v.status == Status.Active) totalActiveStake += msg.value;
    }

    /// Emergency brake on new stake (the only power the multisig has without a timelock).
    function setDepositsPaused(bool paused) external {
        if (msg.sender != Sys.SAFE) revert Unauthorized();
        depositsPaused = paused;
        emit DepositsPaused(paused);
    }

    // ---------------------------------------------------------------- views

    /// Active validators with at least the minimum stake: the committee lottery input. The order is
    /// unspecified; the node sorts by id.
    function snapshot() external view returns (uint32[] memory ids, uint256[] memory stakes) {
        uint256 n = active.length;
        ids = new uint32[](n);
        stakes = new uint256[](n);
        uint256 k;
        for (uint256 i = 0; i < n; i++) {
            uint32 id = active[i];
            uint256 s = validators[id].stake;
            if (s >= Sys.MIN_STAKE) {
                ids[k] = id;
                stakes[k] = s;
                k++;
            }
        }
        assembly {
            mstore(ids, k)
            mstore(stakes, k)
        }
    }

    /// Bootstrap validators still active (the committee while the bootstrap phase lasts).
    function bootstrapSet() external view returns (uint32[] memory ids) {
        uint256 n = bootstrapIds.length;
        ids = new uint32[](n);
        uint256 k;
        for (uint256 i = 0; i < n; i++) {
            if (validators[bootstrapIds[i]].status == Status.Active) ids[k++] = bootstrapIds[i];
        }
        assembly {
            mstore(ids, k)
        }
    }

    /// Stakers counted for the bootstrap exit condition (active, at least the minimum stake).
    function stakerStats() external view returns (uint256 stakers, uint256 total) {
        for (uint256 i = 0; i < active.length; i++) {
            uint256 s = validators[active[i]].stake;
            if (s >= Sys.MIN_STAKE) {
                stakers++;
                total += s;
            }
        }
    }

    function pubkeyOf(uint32 id) public view returns (bytes memory) {
        Validator storage v = validators[id];
        return abi.encodePacked(v.pk0, v.pk1);
    }

    /// Public keys and fee recipients for a list of ids (used by the node to build committees).
    function keysOf(uint32[] calldata ids) external view returns (bytes memory pubkeys, address[] memory recipients) {
        recipients = new address[](ids.length);
        for (uint256 i = 0; i < ids.length; i++) {
            Validator storage v = validators[ids[i]];
            pubkeys = abi.encodePacked(pubkeys, v.pk0, v.pk1);
            recipients[i] = v.feeRecipient;
        }
    }

    function validator(uint32 id)
        external
        view
        returns (
            address owner,
            address feeRecipient,
            uint256 stake,
            uint256 locked,
            Status status,
            bool bootstrap,
            uint64 exitEpoch,
            bytes memory pubkey
        )
    {
        Validator storage v = validators[id];
        return (v.owner, v.feeRecipient, v.stake, v.locked, v.status, v.bootstrap, v.exitEpoch, pubkeyOf(id));
    }

    function isBootstrap(uint32 id) external view returns (bool) {
        return validators[id].bootstrap;
    }

    // ---------------------------------------------------------------- internal

    function _create(bytes calldata pubkey, address owner, address feeRecipient, uint256 stake)
        private
        returns (uint32 id)
    {
        if (pubkey.length != 48) revert BadKey();
        bytes32 h = keccak256(pubkey);
        if (idOfPubkey[h] != 0) revert KeyTaken();
        id = ++count;
        idOfPubkey[h] = id;
        Validator storage v = validators[id];
        v.owner = owner;
        v.feeRecipient = feeRecipient;
        v.stake = uint128(stake);
        v.status = Status.Active;
        v.pk0 = bytes32(pubkey[0:32]);
        v.pk1 = bytes16(pubkey[32:48]);
        active.push(id);
        activePos[id] = uint32(active.length);
        emit Registered(id, owner, pubkey, feeRecipient, stake);
    }

    function _deactivate(uint32 id, uint256 stake) private {
        uint32 pos = activePos[id];
        uint32 last = active[active.length - 1];
        active[pos - 1] = last;
        activePos[last] = pos;
        active.pop();
        delete activePos[id];
        totalActiveStake -= stake;
    }

    function _epoch() private view returns (uint64) {
        return IConsensusRegistry(Sys.CONSENSUS).currentEpoch();
    }
}
