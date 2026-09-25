// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {ERC1967Proxy} from "@openzeppelin/contracts/proxy/ERC1967/ERC1967Proxy.sol";

/// @title ERC-1967 proxy placed at each system contract address at genesis.
contract SystemProxy is ERC1967Proxy {
    constructor(address implementation) ERC1967Proxy(implementation, "") {}

    /// Deployed only inside genesis construction, where initialisation follows atomically (no
    /// transaction can run in between), so the front-running risk this guard exists for does not
    /// apply. Genesis-only initialisers are protected by `onlyGenesis` instead.
    function _unsafeAllowUninitialized() internal pure override returns (bool) {
        return true;
    }
}
