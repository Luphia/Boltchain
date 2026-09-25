// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {Sys, SystemContract} from "./System.sol";
import {BLS} from "./BLS.sol";

interface IStaking {
    function pubkeyOf(uint32 id) external view returns (bytes memory);
    function slash(uint32 id, uint256 bps, address reporter) external returns (uint256);
    function totalActiveStake() external view returns (uint256);
}

/// @title Epochs, committees and equivocation evidence.
/// @notice The node writes the committee of epoch e+1 at the first block of epoch e (see ADR 0006),
/// so every committee is fixed one epoch ahead and readable from current state. It also tracks
/// the PoW -> PoS switch (ADR 0007): no committees exist before PoS is scheduled.
contract ConsensusRegistry is SystemContract {
    struct Committee {
        uint32[] members; // validator ids, in consensus index order
        uint16[] weights; // seats per member
        bytes seats; // member index (uint16, big-endian) of each seat, in leader order
    }

    /// Committees older than this many epochs are deleted.
    uint64 public constant KEEP_EPOCHS = 32;
    uint256 public constant BASE_SLASH_BPS = 500; // 5%

    uint64 public currentEpoch;
    /// Whether the switch to PoS is decided (phase C from `posEpoch` on, ADR 0007 §5).
    bool public posScheduled;
    /// First PoS epoch, once scheduled. Blocks of earlier epochs are mined (PoW).
    uint64 public posEpoch;
    /// Consecutive epoch starts at which the PoS thresholds were met.
    uint64 public thresholdStreak;
    mapping(uint64 => Committee) internal committees;
    mapping(bytes32 => bool) public evidenceUsed;
    // Slashed amounts per epoch, in a ring of 16 (covers the 14-epoch correlation window).
    uint256[16] internal slashedRing;
    uint64[16] internal slashedRingEpoch;

    event EpochStarted(uint64 indexed epoch, uint64 thresholdStreak);
    event PosScheduled(uint64 indexed posEpoch);
    event CommitteeSet(uint64 indexed epoch, bytes32 committeeHash);
    event Equivocation(uint32 indexed validator, uint64 epoch, uint64 round, uint256 slashed, address reporter);

    error BadEvidence();
    error EvidenceTooOld();
    error EvidenceUsed();

    // Signed-message layouts (must match crates/consensus/src/types.rs).
    bytes17 private constant VOTE_TAG = "boltchain/vote/v2";
    bytes21 private constant PROPOSAL_TAG = "boltchain/proposal/v2";
    uint256 private constant VOTE_LEN = 17 + 24 + 32;
    uint256 private constant PROPOSAL_LEN = 21 + 24 + 64;

    /// Dev chains with genesis validators only: PoS from genesis, with the committee of epoch 0
    /// (block 1 draws epoch 1's). A regular genesis leaves the registry empty (PoW until the
    /// thresholds are met).
    function initialize(uint32[] calldata memberIds, uint16[] calldata weights, bytes calldata seats)
        external
        onlyGenesis
    {
        posScheduled = true;
        _store(0, memberIds, weights, seats);
    }

    /// First block of epoch `epoch`. `thresholdMet`: whether stake met the PoS thresholds (T2)
    /// in the parent state. After `streakRequired` consecutive epochs, PoS is scheduled to start two
    /// epochs later, so its first committee can be drawn one epoch ahead like every other.
    function beginEpoch(uint64 epoch, bool thresholdMet, uint64 streakRequired)
        external
        onlySystem
        returns (bool scheduled, uint64 firstPosEpoch)
    {
        currentEpoch = epoch;
        if (!posScheduled) {
            thresholdStreak = thresholdMet ? thresholdStreak + 1 : 0;
            if (thresholdStreak >= streakRequired) {
                posScheduled = true;
                posEpoch = epoch + 2;
                emit PosScheduled(epoch + 2);
            }
        }
        if (epoch + 1 >= KEEP_EPOCHS) {
            delete committees[epoch + 1 - KEEP_EPOCHS];
        }
        emit EpochStarted(epoch, thresholdStreak);
        return (posScheduled, posEpoch);
    }

    /// Stores the committee of `epoch` (drawn by the node at the start of `epoch - 1`).
    function setCommittee(uint64 epoch, uint32[] calldata memberIds, uint16[] calldata weights, bytes calldata seats)
        external
        onlySystem
    {
        _store(epoch, memberIds, weights, seats);
        emit CommitteeSet(epoch, keccak256(abi.encode(memberIds, weights, seats)));
    }

    /// (PoS scheduled, first PoS epoch, threshold streak).
    function phase() external view returns (bool, uint64, uint64) {
        return (posScheduled, posEpoch, thresholdStreak);
    }

    function committee(uint64 epoch)
        external
        view
        returns (uint32[] memory ids, uint16[] memory weights, bytes memory seats)
    {
        Committee storage c = committees[epoch];
        return (c.members, c.weights, c.seats);
    }

    function members(uint64 epoch) external view returns (uint32[] memory ids, uint16[] memory weights) {
        Committee storage c = committees[epoch];
        return (c.members, c.weights);
    }

    /// Slashes validator `id` for signing two conflicting votes, or two conflicting proposals, in
    /// the same epoch and round. `pubkeyPoint` is its key as an EIP-2537 G1 point; the signatures
    /// are EIP-2537 G2 points.
    function submitEvidence(
        uint32 id,
        bytes calldata pubkeyPoint,
        bytes calldata msgA,
        bytes calldata sigA,
        bytes calldata msgB,
        bytes calldata sigB
    ) external returns (uint256 slashed) {
        bytes memory pk = IStaking(Sys.STAKING).pubkeyOf(id);
        if (pk.length != 48 || keccak256(BLS.compressG1(pubkeyPoint)) != keccak256(pk)) revert BadEvidence();
        (uint64 epoch, uint64 round) = _conflict(msgA, msgB);
        if (epoch + Sys.UNBONDING_EPOCHS < currentEpoch) revert EvidenceTooOld();
        bytes32 key = keccak256(abi.encode(id, epoch, round, msgA.length));
        if (evidenceUsed[key]) revert EvidenceUsed();
        if (!BLS.verify(pubkeyPoint, msgA, sigA, BLS.SIG_DST) || !BLS.verify(pubkeyPoint, msgB, sigB, BLS.SIG_DST)) {
            revert BadEvidence();
        }
        evidenceUsed[key] = true;
        slashed = IStaking(Sys.STAKING).slash(id, _slashBps(), msg.sender);
        _recordSlash(slashed);
        emit Equivocation(id, epoch, round, slashed, msg.sender);
    }

    /// Current slashing rate: 5% plus three times the share of stake slashed in the last 14
    /// epochs, capped at 100%.
    function slashBps() external view returns (uint256) {
        return _slashBps();
    }

    // ---------------------------------------------------------------- internal

    function _store(uint64 epoch, uint32[] calldata m, uint16[] calldata w, bytes calldata s) private {
        Committee storage c = committees[epoch];
        c.members = m;
        c.weights = w;
        c.seats = s;
    }

    /// Returns (epoch, round) if the two messages are conflicting votes or proposals on this chain.
    function _conflict(bytes calldata a, bytes calldata b) private view returns (uint64 epoch, uint64 round) {
        if (a.length != b.length || keccak256(a) == keccak256(b)) revert BadEvidence();
        uint256 tagLen;
        if (a.length == VOTE_LEN && bytes17(a[0:17]) == VOTE_TAG && bytes17(b[0:17]) == VOTE_TAG) {
            tagLen = 17;
        } else if (
            a.length == PROPOSAL_LEN && bytes21(a[0:21]) == PROPOSAL_TAG && bytes21(b[0:21]) == PROPOSAL_TAG
        ) {
            tagLen = 21;
        } else {
            revert BadEvidence();
        }
        // chain id, epoch and round must match; the remainder (block hash, payload hash) differs.
        if (keccak256(a[tagLen:tagLen + 24]) != keccak256(b[tagLen:tagLen + 24])) revert BadEvidence();
        if (uint64(bytes8(a[tagLen:tagLen + 8])) != block.chainid) revert BadEvidence();
        epoch = uint64(bytes8(a[tagLen + 8:tagLen + 16]));
        round = uint64(bytes8(a[tagLen + 16:tagLen + 24]));
    }

    function _slashBps() private view returns (uint256) {
        uint256 total = IStaking(Sys.STAKING).totalActiveStake();
        if (total == 0) return 10_000;
        uint256 recent;
        for (uint256 i = 0; i < 16; i++) {
            if (slashedRingEpoch[i] + Sys.UNBONDING_EPOCHS >= currentEpoch) recent += slashedRing[i];
        }
        uint256 bps = BASE_SLASH_BPS + 3 * recent * 10_000 / total;
        return bps > 10_000 ? 10_000 : bps;
    }

    function _recordSlash(uint256 amount) private {
        uint256 i = currentEpoch % 16;
        if (slashedRingEpoch[i] != currentEpoch) {
            slashedRingEpoch[i] = currentEpoch;
            slashedRing[i] = 0;
        }
        slashedRing[i] += amount;
    }
}
