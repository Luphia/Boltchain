// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {Sys, SystemContract} from "./System.sol";

interface IRegistry {
    function members(uint64 epoch) external view returns (uint32[] memory ids, uint16[] memory weights);
}

interface IHistoryRewards {
    function passed(uint64 epoch) external view returns (uint32[] memory);
}

interface IStakingRewards {
    function validator(uint32 id) external view returns (address, address, uint256, uint8, uint64, bytes memory);
}

/// @title Circulating supply, participation records and epoch rewards.
/// @notice New BOLT enters in two ways: while blocks are mined, the node credits each block's
/// reward to its miner and reports it in `onBlock`; under PoS, the node credits this contract with
/// the epoch's consensus emission in `settle`, which pays it out by participation (what is not
/// paid stays in the unissued pool). Burned base fees and slashed stake leave the supply.
contract RewardDistributor is SystemContract {
    /// Circulating supply in wei (cap minus unissued pool).
    uint256 public supply;
    /// Votes per committee member per epoch, as 16 packed 16-bit counters per word (512 members).
    mapping(uint64 => uint256[32]) internal votes;
    /// Claimable rewards per validator id.
    mapping(uint32 => uint256) public rewards;
    /// Highest certificate round whose votes were recorded, per epoch (a certificate counts once,
    /// however many blocks carry it).
    mapping(uint64 => uint64) public lastRecordedRound;
    /// Epochs below this one are settled; later certificates for them are ignored.
    uint64 public settledUpTo;

    event Settled(uint64 indexed epoch, uint256 emission, uint256 paid);
    event StorageSettled(uint64 indexed epoch, uint256 emission, uint256 paid);
    event Claimed(uint32 indexed id, address to, uint256 amount);

    error TooManyMembers();
    error TransferFailed();

    /// Genesis: the balances allocated in genesis (dev chains only; zero on mainnet).
    function initialize(uint256 genesisSupply) external onlyGenesis {
        supply = genesisSupply;
    }

    /// Every block, before its transactions: removes the parent's burned base fee from the supply,
    /// adds the block reward the node credited to a miner (`minted`, zero under PoS) and records
    /// the votes in the block's certificate (`bitmap` indexes the members of `certEpoch`'s
    /// committee, bit i of byte i/8). Under PoS the certificate is the parent's; a mined block
    /// may carry the latest checkpoint certificate (phase B). A round is only counted once.
    function onBlock(uint256 burned, uint256 minted, uint64 certEpoch, uint64 certRound, bytes calldata bitmap)
        external
        onlySystem
    {
        supply = burned > supply ? minted : supply - burned + minted;
        if (bitmap.length > 64) revert TooManyMembers();
        if (bitmap.length == 0 || certEpoch < settledUpTo || certRound <= lastRecordedRound[certEpoch]) return;
        lastRecordedRound[certEpoch] = certRound;
        uint256[32] storage v = votes[certEpoch];
        for (uint256 w = 0; w * 2 < bitmap.length; w++) {
            // members 16w .. 16w+15 are bits of bitmap bytes 2w and 2w+1
            uint256 bits = uint256(uint8(bitmap[2 * w]));
            if (2 * w + 1 < bitmap.length) bits |= uint256(uint8(bitmap[2 * w + 1])) << 8;
            if (bits == 0) continue;
            uint256 word = v[w];
            for (uint256 k = 0; k < 16; k++) {
                if (bits & (1 << k) != 0) word += 1 << (16 * k);
            }
            v[w] = word;
        }
    }

    /// First block of epoch `epoch + 1`: pays epoch `epoch`'s committee in proportion to votes x
    /// seats out of `emission` (nothing for a mined epoch, which has no committee). The node first
    /// calls `preview` and credits exactly the returned amount to this contract; what is not paid
    /// stays in the unissued pool.
    function settle(uint64 epoch, uint256 emission) external onlySystem returns (uint256 paid) {
        (uint32[] memory ids, uint256[] memory shares, uint256 total) = _shares(epoch, emission);
        for (uint256 i = 0; i < ids.length; i++) {
            if (shares[i] != 0) rewards[ids[i]] += shares[i];
        }
        paid = total;
        delete votes[epoch];
        delete lastRecordedRound[epoch];
        if (epoch + 1 > settledUpTo) settledUpTo = epoch + 1;
        supply += paid;
        emit Settled(epoch, emission, paid);
    }

    /// First block of epoch `epoch + 1`: the storage share of the emission, split equally among
    /// the audit passes of epoch `epoch` (a provider that passed two audits gets two shares). The
    /// node credits exactly `previewStorage` first; nothing is paid without passes.
    function settleStorage(uint64 epoch, uint256 emission) external onlySystem returns (uint256 paid) {
        uint32[] memory ids = IHistoryRewards(Sys.HISTORY).passed(epoch);
        if (ids.length == 0) return 0;
        uint256 share = emission / ids.length;
        for (uint256 i = 0; i < ids.length; i++) {
            rewards[ids[i]] += share;
        }
        paid = share * ids.length;
        supply += paid;
        emit StorageSettled(epoch, emission, paid);
    }

    /// Amount `settleStorage(epoch, emission)` would pay.
    function previewStorage(uint64 epoch, uint256 emission) external view returns (uint256) {
        uint256 n = IHistoryRewards(Sys.HISTORY).passed(epoch).length;
        return n == 0 ? 0 : emission / n * n;
    }

    /// Amount `settle(epoch, emission)` would pay.
    function preview(uint64 epoch, uint256 emission) external view returns (uint256 paid) {
        (,, paid) = _shares(epoch, emission);
    }

    function _shares(uint64 epoch, uint256 emission)
        private
        view
        returns (uint32[] memory ids, uint256[] memory shares, uint256 paid)
    {
        uint16[] memory weights;
        (ids, weights) = IRegistry(Sys.CONSENSUS).members(epoch);
        uint256[32] storage v = votes[epoch];
        shares = new uint256[](ids.length);
        uint256 total;
        for (uint256 i = 0; i < ids.length && i < 512; i++) {
            shares[i] = ((v[i / 16] >> (16 * (i % 16))) & 0xffff) * weights[i];
            total += shares[i];
        }
        if (total == 0) return (ids, shares, 0);
        for (uint256 i = 0; i < ids.length; i++) {
            if (shares[i] == 0) continue;
            uint256 share = emission * shares[i] / total;
            shares[i] = share;
            paid += share;
        }
    }

    /// Sends validator `id`'s rewards to its fee recipient (anyone may trigger).
    function claim(uint32 id) external returns (uint256 amount) {
        amount = rewards[id];
        rewards[id] = 0;
        (, address to,,,,) = IStakingRewards(Sys.STAKING).validator(id);
        emit Claimed(id, to, amount);
        (bool ok,) = to.call{value: amount}("");
        if (!ok) revert TransferFailed();
    }

    /// Slashed stake leaves the circulating supply.
    function burnSupply(uint256 amount) external {
        if (msg.sender != Sys.STAKING) revert Unauthorized();
        supply = amount > supply ? 0 : supply - amount;
    }

    /// Votes recorded for member `index` of epoch `epoch`'s committee.
    function votesOf(uint64 epoch, uint256 index) external view returns (uint256) {
        return (votes[epoch][index / 16] >> (16 * (index % 16))) & 0xffff;
    }
}
