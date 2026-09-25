// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {UUPSUpgradeable} from "@openzeppelin/contracts/proxy/utils/UUPSUpgradeable.sol";

/// @title Fixed addresses and protocol constants shared by the Boltchain system contracts.
library Sys {
    /// Caller of node-issued system calls (as EIP-4788 / EIP-2935).
    address internal constant SYSTEM = 0xffffFFFfFFffffffffffffffFfFFFfffFFFfFFfE;

    address internal constant STAKING = 0xB017000000000000000000000000000000000001;
    address internal constant CONSENSUS = 0xB017000000000000000000000000000000000002;
    address internal constant REWARDS = 0xb017000000000000000000000000000000000003;
    address internal constant PARAMS = 0xb017000000000000000000000000000000000004;
    address internal constant HISTORY = 0xb017000000000000000000000000000000000005;

    /// Governance multisig (Safe, 5-of-9). Holds the emergency deposit pause.
    address internal constant SAFE = 0xb0170000000000000000000000000000000000a0;
    /// 16-day timelock: system contract upgrades and security-relevant parameters.
    address internal constant UPGRADE_TIMELOCK = 0xb0170000000000000000000000000000000000a1;
    /// 2-day timelock: bounded parameter adjustments.
    address internal constant PARAM_TIMELOCK = 0xb0170000000000000000000000000000000000a2;

    uint256 internal constant MIN_STAKE = 64 ether;
    /// Unbonding period, in epochs (14 days); also the evidence window.
    uint64 internal constant UNBONDING_EPOCHS = 14;
}

/// @title Base for upgradeable system contracts: UUPS, upgradable only by the 16-day timelock.
abstract contract SystemContract is UUPSUpgradeable {
    error Unauthorized();

    modifier onlySystem() {
        if (msg.sender != Sys.SYSTEM) revert Unauthorized();
        _;
    }

    /// Genesis-only initialisation, executed by the node as the system address at block 0.
    modifier onlyGenesis() {
        if (msg.sender != Sys.SYSTEM || block.number != 0) revert Unauthorized();
        _;
    }

    function _authorizeUpgrade(address) internal view override {
        if (msg.sender != Sys.UPGRADE_TIMELOCK) revert Unauthorized();
    }
}
