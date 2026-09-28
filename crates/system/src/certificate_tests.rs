//! Evidence certificates (ADR 0016): the Merkle root of the files, codes, the storage deal the fee
//! pays for, and what cannot be done twice.

use crate::{abi::*, addresses::*, artifacts, tests::Evm};
use alloy_primitives::{Address, B256, Bytes, FixedBytes, U256, keccak256};
use alloy_sol_types::{SolCall, SolValue};

const CERTS: Address = Address::new([0xce; 20]);
const GIB_PRICE: u128 = 1_000_000_000_000_000_000;

fn bolt(n: u64) -> U256 {
    U256::from(n) * U256::from(10u128.pow(18))
}

fn setup(providers: u8) -> (Evm, Address) {
    let mut evm = Evm::from_genesis(&crate::tests::dev_genesis());
    evm.timestamp = evm.timestamp.max(1_000_000);
    evm.system(
        CONSENSUS,
        IConsensusRegistry::beginEpochCall {
            epoch: 10,
            checkpointMet: false,
            posMet: false,
            streakRequired: 1000,
        },
    );
    for i in 0..providers {
        let account = Address::repeat_byte(0x40 + i);
        evm.fund(account, bolt(2));
        let id = evm.call(
            account,
            SWARM,
            U256::ZERO,
            ISwarmStorage::registerStorageCall { peerId: Bytes::from(vec![i; 4]) },
        );
        evm.call(account, SWARM, bolt(1), ISwarmStorage::depositBondCall { id });
        evm.call(
            account,
            SWARM,
            U256::ZERO,
            ISwarmStorage::offerStorageCall { id, capacityMiB: 1024, minPrice: GIB_PRICE / 2 },
        );
    }
    evm.deploy_at(CERTS, &artifacts::certificates().deployed);
    let user = Address::repeat_byte(0xd0);
    evm.fund(user, bolt(1_000));
    (evm, user)
}

/// The same tree in Rust: prefixed leaves, prefixed nodes, lone node carried up.
fn root_of(files: &[(B256, B256, u64)]) -> B256 {
    let mut layer: Vec<B256> = files
        .iter()
        .map(|(k, s, n)| {
            let content = keccak256((*k, *s, *n).abi_encode());
            keccak256((FixedBytes::<1>::from([0u8]), content).abi_encode())
        })
        .collect();
    while layer.len() > 1 {
        layer = layer
            .chunks(2)
            .map(|p| match p {
                [a, b] => keccak256((FixedBytes::<1>::from([1u8]), *a, *b).abi_encode()),
                [a] => *a,
                _ => unreachable!(),
            })
            .collect();
    }
    layer[0]
}

fn files(n: u8) -> Vec<(B256, B256, u64)> {
    (0..n).map(|i| (keccak256([i]), keccak256([i, i]), 1000 + u64::from(i))).collect()
}

fn issue_call(files: &[(B256, B256, u64)], salt: u8, deal_root: &[u8]) -> ICertificates::issueCall {
    ICertificates::issueCall {
        salt: B256::repeat_byte(salt),
        k: files.iter().map(|f| f.0).collect(),
        s: files.iter().map(|f| f.1).collect(),
        sizes: files.iter().map(|f| f.2).collect(),
        publicFiles: true,
        manifest: Bytes::from_static(b"manifest-cid"),
        dealRoot: Bytes::copy_from_slice(deal_root),
        blocks: 5,
        size: 1 << 20,
        replicas: 3,
        epochs: 100,
        price: GIB_PRICE,
    }
}

/// 1 MiB at 1 BOLT per GiB for 3 copies and 100 epochs.
fn cost() -> U256 {
    (U256::from(GIB_PRICE) + U256::from(1023)) / U256::from(1024) * U256::from(300)
}

#[test]
fn the_root_matches_and_a_certificate_pays_its_deal() {
    let (mut evm, user) = setup(4);
    for n in 1..=9u8 {
        let f = files(n);
        let c = ICertificates::rootOfCall {
            k: f.iter().map(|x| x.0).collect(),
            s: f.iter().map(|x| x.1).collect(),
            sizes: f.iter().map(|x| x.2).collect(),
        };
        assert_eq!(evm.view(CERTS, c), root_of(&f), "{n} files");
    }

    let f = files(3);
    let root = root_of(&f);
    // The same values are checked in the explorer's verify.js tests.
    assert_eq!(
        root,
        alloy_primitives::b256!("375eda9e3fe392f99312159cfa518829c276e41b43cad1f60542ee22d31b3e72")
    );
    let code = evm
        .view(CERTS, ICertificates::codeOfCall { root, issuer: user, salt: B256::repeat_byte(7) });
    assert_eq!(code, alloy_primitives::fixed_bytes!("29edf803c3d2d84b57d0"));
    let got = evm.call(user, CERTS, cost(), issue_call(&f, 7, b"deal-root-1"));
    assert_eq!(got, code);
    let c = evm.view(CERTS, ICertificates::certificateCall { code });
    assert_eq!((c.issuer, c.files, c.publicFiles, c.root), (user, 3, true, root));
    assert_eq!(c.manifest, Bytes::from_static(b"manifest-cid"));
    let d = evm.view(SWARM, ISwarmStorage::dealCall { id: c.deal });
    assert_eq!(d.owner, CERTS, "the contract owns the deal, so nobody can cancel it");
    assert_eq!((d.replicas, d.endEpoch - d.startEpoch), (3, 100));
    assert_eq!(
        evm.view(CERTS, ICertificates::codeOfDealCall { dealRootHash: keccak256(b"deal-root-1") }),
        code
    );
    assert_eq!(evm.view(CERTS, ICertificates::countCall {}), U256::from(1));
    assert!(
        !evm.try_call(user, SWARM, U256::ZERO, ISwarmStorage::cancelDealCall { id: c.deal }),
        "the issuer is not the deal owner"
    );

    // Anyone can extend the deal.
    let other = Address::repeat_byte(0xd1);
    evm.fund(other, bolt(10));
    let per_epoch = cost() / U256::from(300) * U256::from(3);
    evm.call(
        other,
        CERTS,
        per_epoch * U256::from(10),
        ICertificates::extendCall { code, epochs: 10 },
    );
    let d = evm.view(SWARM, ISwarmStorage::dealCall { id: c.deal });
    assert_eq!(d.endEpoch - d.startEpoch, 110);
}

#[test]
fn what_cannot_be_issued() {
    let (mut evm, user) = setup(4);
    let f = files(2);
    assert!(evm.try_call(user, CERTS, cost(), issue_call(&f, 1, b"root-a")));
    // Same files, issuer and salt: same code.
    assert!(!evm.try_call(user, CERTS, cost(), issue_call(&f, 1, b"root-b")), "code taken");
    // A deal index can back only one certificate.
    assert!(!evm.try_call(user, CERTS, cost(), issue_call(&f, 2, b"root-a")), "deal root reused");
    // Too little for the deal.
    assert!(
        !evm.try_call(user, CERTS, cost() - U256::from(1), issue_call(&f, 3, b"root-c")),
        "underpaid"
    );
    // No files, too many files, mismatched arrays.
    assert!(!evm.try_call(user, CERTS, cost(), issue_call(&[], 4, b"root-d")));
    assert!(!evm.try_call(
        user,
        CERTS,
        cost(),
        issue_call(&files(255).into_iter().cycle().take(257).collect::<Vec<_>>(), 4, b"root-e")
    ));
    let mut bad = issue_call(&f, 5, b"root-f");
    bad.sizes.pop();
    assert!(!evm.try_call(user, CERTS, cost(), bad));
    // Not enough storage providers for 3 copies.
    let (mut evm2, user2) = setup(2);
    assert!(!evm2.try_call(user2, CERTS, cost(), issue_call(&f, 1, b"root-a")), "no providers");
    // Unknown code cannot be extended.
    assert!(!evm.try_call(
        user,
        CERTS,
        U256::from(1),
        ICertificates::extendCall { code: FixedBytes::ZERO, epochs: 1 }
    ));
    let _ = ICertificates::issueCall::SELECTOR;
}
