// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {SystemContract} from "./System.sol";

/// @title History registry (epoch index CIDs, snapshot CIDs, shard assignments, audits).
/// @notice Placeholder that reserves the address; the storage-audit logic arrives in M5 through a
/// hard fork that replaces this code (ADR 0008 §2).
contract HistoryRegistry is SystemContract {
    function version() external pure returns (uint256) {
        return 0;
    }
}
