// SPDX-License-Identifier: MIT
pragma solidity ^0.8.30;

interface ISwarmDeals {
    function createDeal(bytes calldata root, uint64 blocks, uint64 size, uint8 replicas, uint64 epochs, uint128 price)
        external
        payable
        returns (uint256 id);
    function extendDeal(uint256 id, uint64 epochs) external payable;
}

/// @title Certificates
/// @notice Evidence certificates (ADR 0016): a set of files is fixed on chain by the hashes of its
/// files, under a short code people can look up in the explorer. The fee pays a SwarmStorage deal
/// (ADR 0014) that keeps the certificate's manifest and, for a public certificate, the files
/// themselves. Not a system contract: an ordinary contract with no owner, no admin and no way to
/// change or delete a certificate.
///
/// Each file is a leaf `keccak256(abi.encode(bytes1(0x00), keccak256(abi.encode(k, s, size))))`
/// where `k` and `s` are the file's keccak-256 and sha-256; nodes are
/// `keccak256(abi.encode(bytes1(0x01), left, right))`, and a lone node is carried up (the explorer's
/// `prefixed-abi-v1` scheme). File names are never recorded.
contract Certificates {
    ISwarmDeals public constant SWARM = ISwarmDeals(0xb017000000000000000000000000000000000005);
    uint256 public constant MAX_FILES = 256;

    struct Certificate {
        address issuer;
        uint64 issuedAt; // unix seconds
        uint64 blockNumber;
        uint32 files;
        bool publicFiles; // the files themselves are kept and can be downloaded
        bytes32 root; // Merkle root of the file leaves
        uint256 deal; // SwarmStorage deal id
        bytes manifest; // CID of the manifest (listed in the deal)
    }

    mapping(bytes10 => Certificate) internal _certs;
    /// keccak256(deal index root CID) => certificate code
    mapping(bytes32 => bytes10) public codeOfDeal;
    uint256 public count;

    event Issued(
        bytes10 indexed code,
        address indexed issuer,
        bytes32 root,
        uint256 deal,
        bool publicFiles,
        bytes32[] keccak,
        bytes32[] sha,
        uint64[] sizes,
        bytes manifest,
        bytes dealRoot
    );
    event Extended(bytes10 indexed code, uint64 epochs);

    error BadFiles();
    error CodeTaken();
    error DealRootUsed();
    error UnknownCode();

    /// The code a certificate gets: the first 10 bytes of keccak256(root, issuer, salt), shown as
    /// 16 Crockford base-32 characters. The issuer picks `salt`, so the code is known before paying.
    function codeOf(bytes32 root, address issuer, bytes32 salt) public pure returns (bytes10) {
        return bytes10(keccak256(abi.encode(root, issuer, salt)));
    }

    function leafOf(bytes32 k, bytes32 s, uint64 size) public pure returns (bytes32) {
        return keccak256(abi.encode(bytes1(0x00), keccak256(abi.encode(k, s, size))));
    }

    /// Merkle root of the files (see the contract notes).
    function rootOf(bytes32[] calldata k, bytes32[] calldata s, uint64[] calldata sizes) public pure returns (bytes32) {
        uint256 n = k.length;
        if (n == 0 || n > MAX_FILES || s.length != n || sizes.length != n) revert BadFiles();
        bytes32[] memory layer = new bytes32[](n);
        for (uint256 i = 0; i < n; i++) {
            layer[i] = leafOf(k[i], s[i], sizes[i]);
        }
        while (n > 1) {
            uint256 m = (n + 1) / 2;
            for (uint256 i = 0; i < m; i++) {
                uint256 a = 2 * i;
                layer[i] = a + 1 < n ? keccak256(abi.encode(bytes1(0x01), layer[a], layer[a + 1])) : layer[a];
            }
            n = m;
        }
        return layer[0];
    }

    /// Issues a certificate for the files and pays the storage deal with the value sent. The deal
    /// is owned by this contract, so nobody can cancel it.
    function issue(
        bytes32 salt,
        bytes32[] calldata k,
        bytes32[] calldata s,
        uint64[] calldata sizes,
        bool publicFiles,
        bytes calldata manifest,
        bytes calldata dealRoot,
        uint64 blocks,
        uint64 size,
        uint8 replicas,
        uint64 epochs,
        uint128 price
    ) external payable returns (bytes10 code) {
        bytes32 root = rootOf(k, s, sizes);
        code = codeOf(root, msg.sender, salt);
        if (code == bytes10(0) || _certs[code].issuer != address(0)) revert CodeTaken();
        bytes32 dr = keccak256(dealRoot);
        if (codeOfDeal[dr] != bytes10(0)) revert DealRootUsed();
        uint256 deal = SWARM.createDeal{value: msg.value}(dealRoot, blocks, size, replicas, epochs, price);
        _certs[code] = Certificate({
            issuer: msg.sender,
            issuedAt: uint64(block.timestamp),
            blockNumber: uint64(block.number),
            files: uint32(k.length),
            publicFiles: publicFiles,
            root: root,
            deal: deal,
            manifest: manifest
        });
        codeOfDeal[dr] = code;
        count++;
        emit Issued(code, msg.sender, root, deal, publicFiles, k, s, sizes, manifest, dealRoot);
    }

    /// Anyone can pay to keep a certificate's storage deal longer.
    function extend(bytes10 code, uint64 epochs) external payable {
        Certificate storage c = _certs[code];
        if (c.issuer == address(0)) revert UnknownCode();
        SWARM.extendDeal{value: msg.value}(c.deal, epochs);
        emit Extended(code, epochs);
    }

    function certificate(bytes10 code) external view returns (Certificate memory) {
        return _certs[code];
    }

    /// Refunds of closed deals (unused escrow) come back here.
    receive() external payable {}
}
