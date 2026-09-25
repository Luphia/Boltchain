// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

import {BLS} from "../src/BLS.sol";

/// Test-only wrapper exposing the BLS library.
contract BLSHarness {
    function hashToG2(bytes calldata message, bytes calldata dst) external view returns (bytes memory) {
        return BLS.hashToG2(message, dst);
    }

    function expand(bytes calldata message, bytes calldata dst) external pure returns (bytes memory) {
        return BLS.expandMessageXmd(message, dst);
    }

    function verify(bytes calldata pk, bytes calldata message, bytes calldata sig) external view returns (bool) {
        return BLS.verify(pk, message, sig, BLS.SIG_DST);
    }

    function verifyPop(bytes calldata pk, bytes calldata pubkey, bytes calldata pop) external view returns (bool) {
        return BLS.verify(pk, pubkey, pop, BLS.POP_DST);
    }

    function compress(bytes calldata pk) external pure returns (bytes memory) {
        return BLS.compressG1(pk);
    }
}
