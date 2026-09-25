// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {Sys, SystemContract} from "./System.sol";

interface IStakingHistory {
    function validator(uint32 id) external view returns (address, address, uint256, uint8, uint64, bytes memory);
    function penalize(uint32 id, uint256 bps, address reporter) external returns (uint256);
}

/// @title History directory and storage audits (ADR 0009).
/// @notice The first block of every epoch records the previous epoch's index CID (the envelopes of
/// all its blocks), so the whole history can be found from chain state. Epochs older than the
/// recent window are kept by 16 validators each (rendezvous hashing); every epoch the node draws
/// audit tasks and a panel of auditors from the committee. When 2/3 of the panel certify that a
/// provider served (or failed to serve) the sampled data, the certificate is recorded here: a pass
/// earns a share of the storage reward, a failure costs 1% of the stake. All writes come from the
/// node (system calls); the node checks the certificates before calling.
contract HistoryRegistry is SystemContract {
    enum State {
        Pending,
        Passed,
        Failed
    }

    struct Task {
        uint32 provider; // validator id expected to serve the data
        uint64 target; // epoch sampled
        uint64 height; // block whose data is sampled
        State state;
    }

    uint256 public constant AUDIT_SLASH_BPS = 100; // 1%
    /// Audit records older than this many epochs are deleted.
    uint64 public constant KEEP_EPOCHS = 32;

    /// Epoch index CIDs (binary CID bytes).
    mapping(uint64 => bytes) internal indexes;
    /// Number of epochs recorded (they are recorded in order from 0).
    uint64 public indexedEpochs;
    /// libp2p peer id of each validator (for fetching its shards).
    mapping(uint32 => bytes) public peerOf;
    mapping(uint64 => Task[]) internal tasks;
    mapping(uint64 => uint32[]) internal panels;
    /// Providers that passed an audit, once per passed task.
    mapping(uint64 => uint32[]) internal passes;

    event EpochIndexed(uint64 indexed epoch, bytes cid);
    event PeerSet(uint32 indexed id, bytes peerId);
    event AuditsDrawn(uint64 indexed epoch, uint256 tasks);
    event AuditRecorded(uint64 indexed epoch, uint16 task, uint32 provider, bool passed, uint256 penalty);

    error NotOwner();
    error OutOfOrder();
    error BadTask();

    /// First block of epoch `epoch + 1`: the index of epoch `epoch`.
    function recordEpoch(uint64 epoch, bytes calldata cid) external onlySystem {
        if (epoch != indexedEpochs) revert OutOfOrder();
        indexes[epoch] = cid;
        indexedEpochs = epoch + 1;
        emit EpochIndexed(epoch, cid);
    }

    function epochIndex(uint64 epoch) external view returns (bytes memory) {
        return indexes[epoch];
    }

    /// A validator's owner publishes the libp2p peer id its node serves shards from.
    function setPeer(uint32 id, bytes calldata peerId) external {
        (address owner,,,,,) = IStakingHistory(Sys.STAKING).validator(id);
        if (msg.sender != owner) revert NotOwner();
        peerOf[id] = peerId;
        emit PeerSet(id, peerId);
    }

    /// First block of epoch `epoch`: the audit panel (committee members) and tasks.
    function beginAudits(
        uint64 epoch,
        uint32[] calldata panel,
        uint32[] calldata providers,
        uint64[] calldata targets,
        uint64[] calldata heights
    ) external onlySystem {
        if (providers.length != targets.length || targets.length != heights.length) revert BadTask();
        panels[epoch] = panel;
        Task[] storage t = tasks[epoch];
        for (uint256 i = 0; i < providers.length; i++) {
            t.push(Task({provider: providers[i], target: targets[i], height: heights[i], state: State.Pending}));
        }
        if (epoch >= KEEP_EPOCHS) {
            uint64 old = epoch - KEEP_EPOCHS;
            delete tasks[old];
            delete panels[old];
            delete passes[old];
        }
        emit AuditsDrawn(epoch, providers.length);
    }

    /// A certified audit result (checked by the node against the panel's keys). Returns false if
    /// the task was already decided.
    function recordAudit(uint64 epoch, uint16 task, bool ok) external onlySystem returns (bool) {
        Task[] storage ts = tasks[epoch];
        if (task >= ts.length) revert BadTask();
        Task storage t = ts[task];
        if (t.state != State.Pending) return false;
        uint256 penalty;
        if (ok) {
            t.state = State.Passed;
            passes[epoch].push(t.provider);
        } else {
            t.state = State.Failed;
            try IStakingHistory(Sys.STAKING).penalize(t.provider, AUDIT_SLASH_BPS, block.coinbase) returns (
                uint256 p
            ) {
                penalty = p;
            } catch {}
        }
        emit AuditRecorded(epoch, task, t.provider, ok, penalty);
        return true;
    }

    /// Panel, tasks and their states for `epoch`.
    function audits(uint64 epoch)
        external
        view
        returns (
            uint32[] memory panel,
            uint32[] memory providers,
            uint64[] memory targets,
            uint64[] memory heights,
            uint8[] memory states
        )
    {
        Task[] storage ts = tasks[epoch];
        uint256 n = ts.length;
        providers = new uint32[](n);
        targets = new uint64[](n);
        heights = new uint64[](n);
        states = new uint8[](n);
        for (uint256 i = 0; i < n; i++) {
            providers[i] = ts[i].provider;
            targets[i] = ts[i].target;
            heights[i] = ts[i].height;
            states[i] = uint8(ts[i].state);
        }
        return (panels[epoch], providers, targets, heights, states);
    }

    /// Providers that passed audits in `epoch` (one entry per passed task).
    function passed(uint64 epoch) external view returns (uint32[] memory) {
        return passes[epoch];
    }
}
