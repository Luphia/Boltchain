// SPDX-License-Identifier: MIT
pragma solidity 0.8.30;

/// @title BLS12-381 signature verification (Ethereum ciphersuite: public keys in G1, signatures in G2)
/// @notice Uses the EIP-2537 precompiles. Points are in EIP-2537 encoding: every Fp element is
/// 64 bytes (16 zero bytes + 48-byte big-endian value); G1 = x || y (128 bytes);
/// G2 = x.c0 || x.c1 || y.c0 || y.c1 (256 bytes).
/// Hash-to-curve is RFC 9380 BLS12381G2_XMD:SHA-256_SSWU_RO_. The MAP_FP2_TO_G2 precompile clears
/// the cofactor, and cofactor clearing is a group homomorphism, so map(u0) + map(u1) equals the
/// RFC's clear_cofactor(map(u0) + map(u1)).
library BLS {
    /// Signature DST (Ethereum proof-of-possession scheme).
    bytes internal constant SIG_DST = "BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_POP_";
    /// Proof-of-possession DST.
    bytes internal constant POP_DST = "BLS_POP_BLS12381G2_XMD:SHA-256_SSWU_RO_POP_";

    address private constant G2_ADD = address(0x0d);
    address private constant PAIRING = address(0x0f);
    address private constant MAP_FP2_TO_G2 = address(0x11);
    address private constant MODEXP = address(0x05);

    // Field modulus p, as two words of its 64-byte padded encoding.
    bytes32 private constant P_HI = 0x000000000000000000000000000000001a0111ea397fe69a4b1ba7b6434bacd7;
    bytes32 private constant P_LO = 0x64774b84f38512bf6730d2a0f6b0f6241eabfffeb153ffffb9feffffffffaaab;
    // (p - 1) / 2, for the sign bit of compressed points.
    uint256 private constant HALF_HI = 0x0d0088f51cbff34d258dd3db21a5d66b;
    uint256 private constant HALF_LO = 0xb23ba5c279c2895fb39869507b587b120f55ffff58a9ffffdcff7fffffffd555;
    // The G1 generator, negated (x, p - y).
    bytes32 private constant NEG_G1_X_HI = 0x0000000000000000000000000000000017f1d3a73197d7942695638c4fa9ac0f;
    bytes32 private constant NEG_G1_X_LO = 0xc3688c4f9774b905a14e3a3f171bac586c55e83ff97a1aeffb3af00adb22c6bb;
    bytes32 private constant NEG_G1_Y_HI = 0x00000000000000000000000000000000114d1d6855d545a8aa7d76c8cf2e21f2;
    bytes32 private constant NEG_G1_Y_LO = 0x67816aef1db507c96655b9d5caac42364e6f38ba0ecb751bad54dcd6b939c2ca;

    error BadLength();
    error PrecompileFailed();

    /// RFC 9380 expand_message_xmd with SHA-256, producing 256 bytes (two Fp2 elements).
    function expandMessageXmd(bytes memory message, bytes memory dst) internal pure returns (bytes memory out) {
        if (dst.length > 255) revert BadLength();
        bytes memory dstPrime = abi.encodePacked(dst, uint8(dst.length));
        bytes32 b0 = sha256(abi.encodePacked(bytes32(0), bytes32(0), message, uint16(256), uint8(0), dstPrime));
        bytes32 bi = sha256(abi.encodePacked(b0, uint8(1), dstPrime));
        out = new bytes(256);
        assembly {
            mstore(add(out, 32), bi)
        }
        for (uint8 i = 2; i <= 8; i++) {
            bi = sha256(abi.encodePacked(b0 ^ bi, i, dstPrime));
            assembly {
                mstore(add(out, mul(32, i)), bi)
            }
        }
    }

    /// Reduces 64 bytes of `data` starting at `offset` modulo p (EIP-2537 Fp encoding).
    function reduce(bytes memory data, uint256 offset) private view returns (bytes32 hi, bytes32 lo) {
        bytes32 a;
        bytes32 b;
        assembly {
            a := mload(add(add(data, 32), offset))
            b := mload(add(add(data, 64), offset))
        }
        bytes memory input = abi.encodePacked(uint256(64), uint256(1), uint256(64), a, b, uint8(1), P_HI, P_LO);
        (bool ok, bytes memory r) = MODEXP.staticcall(input);
        if (!ok || r.length != 64) revert PrecompileFailed();
        assembly {
            hi := mload(add(r, 32))
            lo := mload(add(r, 64))
        }
    }

    /// Maps one Fp2 element (built from 128 uniform bytes at `offset`) to G2.
    function mapToG2(bytes memory uniform, uint256 offset) private view returns (bytes memory) {
        (bytes32 c0h, bytes32 c0l) = reduce(uniform, offset);
        (bytes32 c1h, bytes32 c1l) = reduce(uniform, offset + 64);
        (bool ok, bytes memory q) = MAP_FP2_TO_G2.staticcall(abi.encodePacked(c0h, c0l, c1h, c1l));
        if (!ok || q.length != 256) revert PrecompileFailed();
        return q;
    }

    /// Hashes `message` to a G2 point (256-byte EIP-2537 encoding).
    function hashToG2(bytes memory message, bytes memory dst) internal view returns (bytes memory) {
        bytes memory uniform = expandMessageXmd(message, dst);
        bytes memory q0 = mapToG2(uniform, 0);
        bytes memory q1 = mapToG2(uniform, 128);
        (bool ok, bytes memory r) = G2_ADD.staticcall(abi.encodePacked(q0, q1));
        if (!ok || r.length != 256) revert PrecompileFailed();
        return r;
    }

    /// Checks e(pk, H(message)) == e(g1, sig). `pk` is a 128-byte G1 point, `sig` a 256-byte G2
    /// point. Returns false for malformed or out-of-subgroup points and for the identity key.
    function verify(bytes memory pk, bytes memory message, bytes memory sig, bytes memory dst)
        internal
        view
        returns (bool)
    {
        if (pk.length != 128 || sig.length != 256 || isZero(pk)) return false;
        bytes memory h = hashToG2(message, dst);
        bytes memory input = abi.encodePacked(pk, h, NEG_G1_X_HI, NEG_G1_X_LO, NEG_G1_Y_HI, NEG_G1_Y_LO, sig);
        // The pairing precompile rejects (fails) points not on the curve or not in the subgroup.
        (bool ok, bytes memory r) = PAIRING.staticcall(input);
        return ok && r.length == 32 && uint256(bytes32(r)) == 1;
    }

    /// Compressed (48-byte, ZCash format) encoding of a 128-byte G1 point.
    function compressG1(bytes memory pk) internal pure returns (bytes memory out) {
        if (pk.length != 128) revert BadLength();
        uint256 xhi;
        uint256 xlo;
        uint256 yhi;
        uint256 ylo;
        assembly {
            xhi := mload(add(pk, 32))
            xlo := mload(add(pk, 64))
            yhi := mload(add(pk, 96))
            ylo := mload(add(pk, 128))
        }
        bool larger = yhi > HALF_HI || (yhi == HALF_HI && ylo > HALF_LO);
        uint256 flags = 0x80 | (larger ? 0x20 : 0);
        // x occupies the low 16 bytes of xhi and all of xlo.
        out = abi.encodePacked(uint128(xhi) | uint128(flags << 120), xlo);
    }

    function isZero(bytes memory b) private pure returns (bool) {
        for (uint256 i = 0; i < b.length; i++) {
            if (b[i] != 0) return false;
        }
        return true;
    }
}
