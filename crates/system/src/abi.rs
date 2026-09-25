//! ABI bindings for the calls the node makes (and tests use).

#![allow(missing_docs)]

use alloy_sol_types::sol;

sol! {
    interface IStakingManager {
        enum Status { None, Active, Exiting, Slashed, Withdrawn }

        function registerGenesis(bytes pubkey, address owner, address feeRecipient, uint256 stake)
            external returns (uint32 id);
        function register(bytes pubkey, bytes pubkeyPoint, bytes pop, address feeRecipient)
            external payable returns (uint32 id);
        function deposit(uint32 id) external payable;
        function requestExit(uint32 id) external;
        function withdraw(uint32 id) external;
        function setFeeRecipient(uint32 id, address feeRecipient) external;

        function snapshot(uint64 minAge) external view returns (uint32[] ids, uint256[] stakes);
        function stakerStats() external view returns (uint256 stakers, uint256 total);
        function maturedStake(uint32 id, uint64 minAge) external view returns (uint256);
        function keysOf(uint32[] ids) external view returns (bytes pubkeys, address[] recipients);
        function pubkeyOf(uint32 id) external view returns (bytes);
        function totalActiveStake() external view returns (uint256);
        function deadStake() external view returns (uint256);
        function penalize(uint32 id, uint256 bps, address reporter) external returns (uint256);
        function count() external view returns (uint32);
        function idOfPubkey(bytes32 h) external view returns (uint32);
        function validator(uint32 id) external view returns (
            address owner, address feeRecipient, uint256 stake, Status status, uint64 exitEpoch,
            bytes pubkey);
    }

    interface IConsensusRegistry {
        function initialize(uint32[] memberIds, uint16[] weights, bytes seats) external;
        function beginEpoch(uint64 epoch, bool checkpointMet, bool posMet, uint64 streakRequired)
            external returns (bool cpScheduled, uint64 firstCheckpointEpoch, bool scheduled, uint64 firstPosEpoch);
        function setCommittee(uint64 epoch, uint32[] memberIds, uint16[] weights, bytes seats) external;
        function phase() external view returns (bool checkpointScheduled, uint64 checkpointEpoch,
            bool posScheduled, uint64 posEpoch, uint64 checkpointStreak, uint64 thresholdStreak);
        function currentEpoch() external view returns (uint64);
        function committee(uint64 epoch) external view returns (uint32[] ids, uint16[] weights, bytes seats);
        function submitEvidence(uint32 id, bytes pubkeyPoint, bytes msgA, bytes sigA, bytes msgB, bytes sigB)
            external returns (uint256 slashed);
        function slashBps() external view returns (uint256);
    }

    interface IRewardDistributor {
        function initialize(uint256 genesisSupply) external;
        function onBlock(uint256 burned, uint256 minted, uint64 certEpoch, uint64 certRound, bytes bitmap)
            external;
        function settle(uint64 epoch, uint256 emission) external returns (uint256 paid);
        function preview(uint64 epoch, uint256 emission) external view returns (uint256 paid);
        function claim(uint32 id) external returns (uint256 amount);
        function supply() external view returns (uint256);
        function rewards(uint32 id) external view returns (uint256);
        function votesOf(uint64 epoch, uint256 index) external view returns (uint256);
        function lastRecordedRound(uint64 epoch) external view returns (uint64);
        function settleStorage(uint64 epoch, uint256 emission) external returns (uint256 paid);
        function previewStorage(uint64 epoch, uint256 emission) external view returns (uint256);
    }

    interface IHistoryRegistry {
        function recordEpoch(uint64 epoch, bytes cid) external;
        function epochIndex(uint64 epoch) external view returns (bytes);
        function indexedEpochs() external view returns (uint64);
        function setPeer(uint32 id, bytes peerId) external;
        function peerOf(uint32 id) external view returns (bytes);
        function beginAudits(uint64 epoch, uint32[] panel, uint32[] providers, uint64[] targets, uint64[] heights)
            external;
        function recordAudit(uint64 epoch, uint16 task, bool ok) external returns (bool);
        function audits(uint64 epoch) external view returns (uint32[] panel, uint32[] providers,
            uint64[] targets, uint64[] heights, uint8[] states);
        function passed(uint64 epoch) external view returns (uint32[]);
    }

    interface IBLSHarness {
        function hashToG2(bytes message, bytes dst) external view returns (bytes);
        function expand(bytes message, bytes dst) external pure returns (bytes);
        function verify(bytes pk, bytes message, bytes sig) external view returns (bool);
        function verifyPop(bytes pk, bytes pubkey, bytes pop) external view returns (bool);
        function compress(bytes pk) external pure returns (bytes);
    }
}
