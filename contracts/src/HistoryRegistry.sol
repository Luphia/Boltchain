// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {Sys, SystemContract} from "./System.sol";

interface IRewardsBurn {
    function burnSupply(uint256 amount) external;
}

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
///
/// Storage providers without stake (ADR 0012): anyone can register an account (ids from
/// 2^31 up), for free and without BOLT (a relayer may submit the signed registration). Each old
/// epoch is kept by 16 validators and, in addition, by 16 of these providers, so they add copies
/// without displacing the staked ones. Their storage rewards build a bond first (20% of each
/// claim, up to 64 BOLT); a failed audit burns 10% of the bond and deactivates the provider.
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
    /// First id of a storage provider without stake (validator ids stay below it).
    uint32 public constant STORAGE_ID_BASE = 2 ** 31;
    uint256 public constant BOND_TARGET = 64 ether;
    uint256 public constant BOND_BPS = 2_000;
    uint256 public constant BOND_SLASH_BPS = 1_000;
    uint64 public constant EXIT_DELAY = 14 days;

    struct StorageProvider {
        address account;
        bool active;
        uint64 exitAt; // bond withdrawable from then on (0: not exiting)
        uint256 bond;
    }
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
    /// Storage providers without stake, by id (`STORAGE_ID_BASE + index`).
    mapping(uint32 => StorageProvider) internal storageProviders;
    /// Ids of all storage providers ever registered, in order.
    uint32[] internal storageIds;
    /// Signed-registration nonces.
    mapping(address => uint256) public nonces;

    event EpochIndexed(uint64 indexed epoch, bytes cid);
    event PeerSet(uint32 indexed id, bytes peerId);
    event AuditsDrawn(uint64 indexed epoch, uint256 tasks);
    event AuditRecorded(uint64 indexed epoch, uint16 task, uint32 provider, bool passed, uint256 penalty);
    event StorageRegistered(uint32 indexed id, address indexed account, bytes peerId);
    event StorageEarned(uint32 indexed id, uint256 paid, uint256 toBond);
    event StorageExit(uint32 indexed id, uint64 exitAt);

    error NotOwner();
    error OutOfOrder();
    error BadTask();
    error Expired();
    error BadSignature();
    error NotStorageProvider();
    error NotExited();
    error TransferFailed();

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

    // ------------------------------------------------------------------ providers without stake

    /// Registers the caller as a storage provider serving from libp2p peer `peerId`.
    function registerStorage(bytes calldata peerId) external returns (uint32) {
        return _registerStorage(msg.sender, peerId);
    }

    /// Registers `account` from its signature, submitted by anyone (it needs no BOLT).
    function registerStorageFor(address account, bytes calldata peerId, uint256 deadline, bytes calldata sig)
        external
        returns (uint32)
    {
        if (block.timestamp > deadline) revert Expired();
        if (sig.length != 65) revert BadSignature();
        bytes32 r = bytes32(sig[0:32]);
        bytes32 s_ = bytes32(sig[32:64]);
        uint8 v = uint8(sig[64]);
        if (v < 27) v += 27;
        if (uint256(s_) > 0x7fffffffffffffffffffffffffffffff5d576e7357a4501ddfe92f46681b20a0) revert BadSignature();
        address got = ecrecover(registrationDigest(account, peerId, deadline), v, r, s_);
        if (got == address(0) || got != account) revert BadSignature();
        nonces[account]++;
        return _registerStorage(account, peerId);
    }

    /// EIP-191 personal-sign hash `account` signs to register (bound to chain, contract, nonce).
    function registrationDigest(address account, bytes calldata peerId, uint256 deadline)
        public
        view
        returns (bytes32)
    {
        bytes32 h = keccak256(
            abi.encode(block.chainid, address(this), keccak256("registerStorage"), account, nonces[account], peerId, deadline)
        );
        return keccak256(abi.encodePacked("\x19Ethereum Signed Message:\n32", h));
    }

    function _registerStorage(address account, bytes calldata peerId) internal returns (uint32 id) {
        id = STORAGE_ID_BASE + uint32(storageIds.length);
        storageIds.push(id);
        storageProviders[id] = StorageProvider({account: account, active: true, exitAt: 0, bond: 0});
        peerOf[id] = peerId;
        emit StorageRegistered(id, account, peerId);
        emit PeerSet(id, peerId);
    }

    /// Active storage providers without stake (the node assigns them history).
    function activeStorageProviders() external view returns (uint32[] memory ids) {
        uint256 n;
        for (uint256 i = 0; i < storageIds.length; i++) {
            if (storageProviders[storageIds[i]].active) n++;
        }
        ids = new uint32[](n);
        n = 0;
        for (uint256 i = 0; i < storageIds.length; i++) {
            if (storageProviders[storageIds[i]].active) ids[n++] = storageIds[i];
        }
    }

    function storageProvider(uint32 id)
        external
        view
        returns (address account, bool active, uint64 exitAt, uint256 bond)
    {
        StorageProvider storage p = storageProviders[id];
        return (p.account, p.active, p.exitAt, p.bond);
    }

    /// Ids registered to `account` (active or not).
    function storageIdsOf(address account) external view returns (uint32[] memory ids) {
        uint256 n;
        for (uint256 i = 0; i < storageIds.length; i++) {
            if (storageProviders[storageIds[i]].account == account) n++;
        }
        ids = new uint32[](n);
        n = 0;
        for (uint256 i = 0; i < storageIds.length; i++) {
            if (storageProviders[storageIds[i]].account == account) ids[n++] = storageIds[i];
        }
    }

    /// The provider's account moves its node to another peer id.
    function setStoragePeer(uint32 id, bytes calldata peerId) external {
        if (storageProviders[id].account != msg.sender) revert NotOwner();
        peerOf[id] = peerId;
        emit PeerSet(id, peerId);
    }

    /// Stops receiving assignments; the bond is withdrawable after `EXIT_DELAY`.
    function exitStorage(uint32 id) external {
        StorageProvider storage p = storageProviders[id];
        if (p.account != msg.sender) revert NotOwner();
        p.active = false;
        p.exitAt = uint64(block.timestamp) + EXIT_DELAY;
        emit StorageExit(id, p.exitAt);
    }

    /// Returns an exited (or deactivated) provider's remaining bond to its account.
    function releaseStorageBond(uint32 id) external {
        StorageProvider storage p = storageProviders[id];
        if (p.active || p.exitAt == 0 || block.timestamp < p.exitAt) revert NotExited();
        uint256 amount = p.bond;
        p.bond = 0;
        (bool ok,) = p.account.call{value: amount}("");
        if (!ok) revert TransferFailed();
    }

    /// RewardDistributor forwards a storage provider's claimed rewards: 20% goes to the bond until
    /// it reaches 64 BOLT, the rest to the account.
    function depositEarnings(uint32 id) external payable {
        if (msg.sender != Sys.REWARDS) revert Unauthorized();
        StorageProvider storage p = storageProviders[id];
        if (p.account == address(0)) revert NotStorageProvider();
        uint256 toBond;
        if (p.bond < BOND_TARGET) {
            toBond = msg.value * BOND_BPS / 10_000;
            if (p.bond + toBond > BOND_TARGET) toBond = BOND_TARGET - p.bond;
            p.bond += toBond;
        }
        uint256 paid = msg.value - toBond;
        emit StorageEarned(id, paid, toBond);
        (bool ok,) = p.account.call{value: paid}("");
        if (!ok) revert TransferFailed();
    }

    // ------------------------------------------------------------------ audits

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
            if (t.provider >= STORAGE_ID_BASE) {
                // Without stake: burn 10% of the bond (it stays locked here) and stop assigning.
                StorageProvider storage sp = storageProviders[t.provider];
                penalty = sp.bond * BOND_SLASH_BPS / 10_000;
                sp.bond -= penalty;
                if (sp.active) {
                    sp.active = false;
                    sp.exitAt = uint64(block.timestamp) + EXIT_DELAY;
                    emit StorageExit(t.provider, sp.exitAt);
                }
                if (penalty > 0) IRewardsBurn(Sys.REWARDS).burnSupply(penalty);
            } else {
                try IStakingHistory(Sys.STAKING).penalize(t.provider, AUDIT_SLASH_BPS, block.coinbase) returns (
                    uint256 p
                ) {
                    penalty = p;
                } catch {}
            }
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
