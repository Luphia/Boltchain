// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {Sys, SystemContract} from "./System.sol";
import {BLS} from "./BLS.sol";

interface IConsensusRegistry {
    function currentEpoch() external view returns (uint64);
}

interface IRewardDistributor {
    function burnSupply(uint256 amount) external;
}

/// @title Validator registry and stake custody.
/// @notice One BLS key per validator, registered with an on-chain verified proof of possession.
/// No delegation (liquid staking can be built on top). Exits take effect after 14 epochs. The same
/// rules apply to every address: no reserved seats, no locked or discounted stake (ADR 0007).
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
        uint128 recent; // part of `stake` deposited in or after `recentEpoch` (not matured yet)
        Status status;
        uint64 exitEpoch; // epoch in which the exit was requested (or the slashing happened)
        uint64 recentEpoch; // epoch of the latest deposit
        bytes32 pk0; // compressed BLS public key, bytes 0..32
        bytes16 pk1; // bytes 32..48
    }

    uint32 public count;
    mapping(uint32 => Validator) internal validators;
    mapping(bytes32 => uint32) public idOfPubkey; // keccak(compressed pubkey) => id
    uint32[] internal active; // Active validators (any stake)
    mapping(uint32 => uint32) internal activePos; // id => index + 1 in `active`
    uint256 public totalActiveStake;
    uint256 public deadStake; // slashed funds kept locked forever (burned)

    event Registered(uint32 indexed id, address indexed owner, bytes pubkey, address feeRecipient, uint256 stake);
    event Deposited(uint32 indexed id, uint256 amount);
    event ExitRequested(uint32 indexed id, uint64 epoch);
    event Withdrawn(uint32 indexed id, uint256 amount);
    event Slashed(uint32 indexed id, uint256 amount, address reporter, uint256 reward);
    event FeeRecipientChanged(uint32 indexed id, address feeRecipient);
    event Penalized(uint32 indexed id, uint256 amount, address reporter, uint256 reward);

    error BelowMinimum();
    error BadKey();
    error KeyTaken();
    error BadProofOfPossession();
    error NotOwner();
    error BadStatus();
    error StillBonded();
    error TransferFailed();

    // ---------------------------------------------------------------- genesis

    /// Dev chains only (the node refuses such a genesis for chain 8017): a validator staked at
    /// genesis. The node credits this contract with `stake` and checked the key's PoP.
    function registerGenesis(bytes calldata pubkey, address owner, address feeRecipient, uint256 stake)
        external
        onlyGenesis
        returns (uint32 id)
    {
        id = _create(pubkey, owner, feeRecipient, stake);
        validators[id].recent = 0; // matured from the start
        totalActiveStake += stake;
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
        if (msg.value < Sys.MIN_STAKE) revert BelowMinimum();
        if (pubkey.length != 48 || keccak256(BLS.compressG1(pubkeyPoint)) != keccak256(pubkey)) revert BadKey();
        if (!BLS.verify(pubkeyPoint, pubkey, pop, BLS.POP_DST)) revert BadProofOfPossession();
        id = _create(pubkey, msg.sender, feeRecipient, msg.value);
        totalActiveStake += msg.value;
    }

    /// Adds stake to an active validator (anyone may top up).
    function deposit(uint32 id) external payable {
        Validator storage v = validators[id];
        if (v.status != Status.Active) revert BadStatus();
        _addRecent(v, msg.value);
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
        uint256 amount = v.stake;
        v.stake = 0;
        v.recent = 0;
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
        if (v.recent > v.stake) v.recent = v.stake;
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

    /// A storage-audit failure (ADR 0009): `bps` of the stake is burned (1% of it to the reporter),
    /// without forcing an exit. Only HistoryRegistry may call.
    function penalize(uint32 id, uint256 bps, address reporter) external returns (uint256 amount) {
        if (msg.sender != Sys.HISTORY) revert Unauthorized();
        Validator storage v = validators[id];
        if (v.status != Status.Active && v.status != Status.Exiting) revert BadStatus();
        amount = uint256(v.stake) * bps / 10_000;
        uint256 reward = amount / 100;
        v.stake -= uint128(amount);
        if (v.recent > v.stake) v.recent = v.stake;
        if (v.status == Status.Active) totalActiveStake -= amount;
        deadStake += amount - reward;
        IRewardDistributor(Sys.REWARDS).burnSupply(amount - reward);
        emit Penalized(id, amount, reporter, reward);
        if (reward > 0) {
            (bool ok,) = reporter.call{value: reward}("");
            if (!ok) deadStake += reward; // an unpayable reporter forfeits its share
        }
    }

    // ---------------------------------------------------------------- views

    /// Active validators whose lottery weight is at least the minimum stake: the committee lottery
    /// input. The weight is the stake deposited at least `minAge` epochs ago (0: all stake; the
    /// first PoS committee uses 14, ADR 0007 §5). The order is unspecified; the node sorts by id.
    function snapshot(uint64 minAge) external view returns (uint32[] memory ids, uint256[] memory stakes) {
        uint256 n = active.length;
        ids = new uint32[](n);
        stakes = new uint256[](n);
        uint64 epoch = _epoch();
        uint256 k;
        for (uint256 i = 0; i < n; i++) {
            uint32 id = active[i];
            uint256 w = _matured(validators[id], epoch, minAge);
            if (w >= Sys.MIN_STAKE) {
                ids[k] = id;
                stakes[k] = w;
                k++;
            }
        }
        assembly {
            mstore(ids, k)
            mstore(stakes, k)
        }
    }

    /// Stakers counted for the phase thresholds (active, at least the minimum stake) and their
    /// total stake.
    function stakerStats() external view returns (uint256 stakers, uint256 total) {
        for (uint256 i = 0; i < active.length; i++) {
            Validator storage v = validators[active[i]];
            if (v.stake >= Sys.MIN_STAKE) {
                stakers++;
                total += v.stake;
            }
        }
    }

    /// Stake of validator `id` deposited at least `minAge` epochs ago.
    function maturedStake(uint32 id, uint64 minAge) external view returns (uint256) {
        return _matured(validators[id], _epoch(), minAge);
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
            Status status,
            uint64 exitEpoch,
            bytes memory pubkey
        )
    {
        Validator storage v = validators[id];
        return (v.owner, v.feeRecipient, v.stake, v.status, v.exitEpoch, pubkeyOf(id));
    }

    // ---------------------------------------------------------------- internal

    /// `recent` covers every deposit since `recentEpoch`; once that is a full unbonding period
    /// old, all of it has matured and the bucket restarts. A new deposit restarts the clock for
    /// the whole bucket, which can only under-count matured stake.
    function _addRecent(Validator storage v, uint256 amount) private {
        uint64 epoch = _epoch();
        if (epoch >= v.recentEpoch + Sys.UNBONDING_EPOCHS) v.recent = 0;
        v.recent += uint128(amount);
        v.recentEpoch = epoch;
    }

    /// `minAge` is at most the unbonding period (a longer one would need more buckets).
    function _matured(Validator storage v, uint64 epoch, uint64 minAge) private view returns (uint256) {
        if (minAge == 0 || epoch >= v.recentEpoch + minAge) return v.stake;
        return v.stake - v.recent;
    }

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
        v.recent = uint128(stake);
        v.recentEpoch = _epoch();
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
