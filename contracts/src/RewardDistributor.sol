// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {Sys, SystemContract} from "./System.sol";

interface IRegistry {
    function members(uint64 epoch) external view returns (uint32[] memory ids, uint16[] memory weights);
    function bootstrapEnded() external view returns (bool);
}

interface IStakingRewards {
    function isBootstrap(uint32 id) external view returns (bool);
    function lockReward(uint32 id) external payable;
    function validator(uint32 id)
        external
        view
        returns (address, address, uint256, uint256, uint8, bool, uint64, bytes memory);
}

/// @title Circulating supply, participation records and epoch rewards.
/// @notice New BOLT only enters through `settle`: the node credits this contract with the epoch's
/// consensus emission, this contract pays it out by participation, and the node debits whatever was
/// not paid (it stays in the unissued pool). Burned base fees and slashed stake leave the supply.
contract RewardDistributor is SystemContract {
    uint256 public constant BOOTSTRAP_REWARD_BPS = 2_500;

    /// Circulating supply in wei (cap minus unissued pool).
    uint256 public supply;
    /// Votes per committee member per epoch, as 16 packed 16-bit counters per word (512 members).
    mapping(uint64 => uint256[32]) internal votes;
    /// Claimable rewards per validator id.
    mapping(uint32 => uint256) public rewards;

    event Settled(uint64 indexed epoch, uint256 emission, uint256 paid);
    event Claimed(uint32 indexed id, address to, uint256 amount);

    error TooManyMembers();
    error TransferFailed();

    /// Genesis: the balances allocated in genesis (dev chains only; zero on mainnet).
    function initialize(uint256 genesisSupply) external onlyGenesis {
        supply = genesisSupply;
    }

    /// Every block, before its transactions: removes the parent's burned base fee from the supply
    /// and records the votes in the block's certificate (`bitmap` indexes the members of
    /// `certEpoch`'s committee, bit i of byte i/8).
    function onBlock(uint256 burned, uint64 certEpoch, bytes calldata bitmap) external onlySystem {
        supply = burned > supply ? 0 : supply - burned;
        if (bitmap.length > 64) revert TooManyMembers();
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
    /// seats out of `emission`. Bootstrap validators get 25% while the bootstrap phase lasts, locked
    /// into their stake. The node first calls `preview` and credits exactly the returned amount to
    /// this contract; what is not paid stays in the unissued pool.
    function settle(uint64 epoch, uint256 emission) external onlySystem returns (uint256 paid) {
        (uint32[] memory ids, uint256[] memory shares, bool[] memory locked, uint256 total) = _shares(epoch, emission);
        for (uint256 i = 0; i < ids.length; i++) {
            if (shares[i] == 0) continue;
            if (locked[i]) {
                IStakingRewards(Sys.STAKING).lockReward{value: shares[i]}(ids[i]);
            } else {
                rewards[ids[i]] += shares[i];
            }
        }
        paid = total;
        delete votes[epoch];
        supply += paid;
        emit Settled(epoch, emission, paid);
    }

    /// Amount `settle(epoch, emission)` would pay.
    function preview(uint64 epoch, uint256 emission) external view returns (uint256 paid) {
        (,,, paid) = _shares(epoch, emission);
    }

    function _shares(uint64 epoch, uint256 emission)
        private
        view
        returns (uint32[] memory ids, uint256[] memory shares, bool[] memory locked, uint256 paid)
    {
        uint16[] memory weights;
        (ids, weights) = IRegistry(Sys.CONSENSUS).members(epoch);
        uint256[32] storage v = votes[epoch];
        shares = new uint256[](ids.length);
        locked = new bool[](ids.length);
        uint256 total;
        for (uint256 i = 0; i < ids.length && i < 512; i++) {
            shares[i] = ((v[i / 16] >> (16 * (i % 16))) & 0xffff) * weights[i];
            total += shares[i];
        }
        if (total == 0) return (ids, shares, locked, 0);
        bool bootstrap = !IRegistry(Sys.CONSENSUS).bootstrapEnded();
        for (uint256 i = 0; i < ids.length; i++) {
            if (shares[i] == 0) continue;
            uint256 share = emission * shares[i] / total;
            if (bootstrap && IStakingRewards(Sys.STAKING).isBootstrap(ids[i])) {
                share = share * BOOTSTRAP_REWARD_BPS / 10_000;
                locked[i] = true;
            }
            shares[i] = share;
            paid += share;
        }
    }

    /// Sends validator `id`'s rewards to its fee recipient (anyone may trigger).
    function claim(uint32 id) external returns (uint256 amount) {
        amount = rewards[id];
        rewards[id] = 0;
        (, address to,,,,,,) = IStakingRewards(Sys.STAKING).validator(id);
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
