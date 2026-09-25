use super::*;
use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_network::TxSignerSync;
use alloy_primitives::TxKind;
use alloy_signer_local::PrivateKeySigner;

fn dev_genesis() -> Genesis {
    Genesis::from_json(include_str!("../../../genesis/dev.json")).unwrap()
}

/// Well-known development key #0 (funded in genesis/dev.json).
fn dev_key() -> PrivateKeySigner {
    "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".parse().unwrap()
}

fn tx(
    signer: &PrivateKeySigner,
    chain_id: u64,
    nonce: u64,
    to: TxKind,
    input: Bytes,
    gas: u64,
) -> TxEnvelope {
    let mut t = TxEip1559 {
        chain_id,
        nonce,
        gas_limit: gas,
        max_fee_per_gas: 1_000_000_000,
        max_priority_fee_per_gas: 1_000_000,
        to,
        value: U256::from(1_000u64),
        input,
        ..Default::default()
    };
    let sig = signer.sign_transaction_sync(&mut t).unwrap();
    t.into_signed(sig).into()
}

#[test]
fn dev_keys_match_genesis_alloc() {
    let g = dev_genesis();
    let addrs: Vec<Address> = [
        "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80",
        "59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
        "5de4111afa1a4b94908f83103eb1f1706367c2e68ca870fc3fb9a804cdab365a",
    ]
    .iter()
    .map(|k| k.parse::<PrivateKeySigner>().unwrap().address())
    .collect();
    for a in addrs {
        assert!(g.alloc.contains_key(&a), "{a} not funded");
    }
}

#[test]
fn produce_then_import_is_deterministic() {
    let g = dev_genesis();
    let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let producer = Chain::open(d1.path(), &g).unwrap();
    let follower = Chain::open(d2.path(), &g).unwrap();
    assert_eq!(producer.head().unwrap().hash_slow(), bolt_system::genesis_hash(&g).unwrap());

    let key = dev_key();
    let cid = g.config.chain_id;
    // Contract that stores CALLVALUE in slot 0 and returns: 34 60 00 55 00
    let deploy_code = Bytes::from_static(&[
        0x60, 0x05, 0x60, 0x0c, 0x60, 0x00, 0x39, 0x60, 0x05, 0x60, 0x00, 0xf3, // initcode
        0x34, 0x60, 0x00, 0x55, 0x00, // runtime
    ]);
    let contract = key.address().create(1);
    let blocks = vec![
        vec![tx(&key, cid, 0, TxKind::Call(Address::repeat_byte(0xb0)), Bytes::new(), 21_000)],
        vec![
            tx(&key, cid, 1, TxKind::Create, deploy_code, 200_000),
            tx(&key, cid, 2, TxKind::Call(Address::repeat_byte(0xb1)), Bytes::new(), 21_000),
        ],
        vec![tx(&key, cid, 3, TxKind::Call(contract), Bytes::new(), 60_000)],
        vec![],
    ];
    for (i, txs) in blocks.into_iter().enumerate() {
        let cands: Vec<_> = txs.iter().cloned().map(|t| (t, key.address())).collect();
        let built =
            producer.build_block(cands, 1_000 + i as u64 * 6, Address::repeat_byte(0xbe)).unwrap();
        assert!(built.rejected.is_empty(), "{:?}", built.rejected);
        let hash = follower.import_block(&built.header, txs).unwrap();
        assert_eq!(hash, built.hash);
        let r = producer.store().reader().unwrap();
        assert_eq!(built.header.state_root, bolt_store::full_state_root(&r).unwrap());
    }
    let r = follower.store().reader().unwrap();
    assert_eq!(r.head().unwrap(), Some(4));
    assert_eq!(
        r.storage(&contract, &B256::ZERO).unwrap(),
        U256::from(1_000u64),
        "contract stored CALLVALUE"
    );
    let receipts = r.receipts(2).unwrap().unwrap();
    assert_eq!(receipts.len(), 2);
    assert!(receipts.iter().all(|x| x.status()));
    assert_eq!(r.tx_location(&blocks_hash_of(&r, 2, 1)).unwrap(), Some((2, 1)));
}

fn blocks_hash_of(r: &bolt_store::Tx<'_, bolt_store::RO>, n: u64, i: usize) -> B256 {
    *r.block(n).unwrap().unwrap().transactions[i].tx_hash()
}

#[test]
fn import_rejects_tampered_block() {
    let g = dev_genesis();
    let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let producer = Chain::open(d1.path(), &g).unwrap();
    let follower = Chain::open(d2.path(), &g).unwrap();
    let key = dev_key();
    let t = tx(
        &key,
        g.config.chain_id,
        0,
        TxKind::Call(Address::repeat_byte(0xb0)),
        Bytes::new(),
        21_000,
    );
    let built = producer.build_block(vec![(t.clone(), key.address())], 10, Address::ZERO).unwrap();
    let mut bad = built.header.clone();
    bad.state_root = B256::repeat_byte(1);
    assert!(matches!(
        follower.import_block(&bad, vec![t.clone()]),
        Err(ChainError::InvalidBlock(_))
    ));
    // Nothing was written; the genuine block still imports.
    assert_eq!(follower.head().unwrap().number, 0);
    follower.import_block(&built.header, vec![t]).unwrap();
}

#[test]
fn reopen_checks_genesis() {
    let g = dev_genesis();
    let d = tempfile::tempdir().unwrap();
    drop(Chain::open(d.path(), &g).unwrap());
    Chain::open(d.path(), &g).unwrap();
    let other = Genesis::from_json(include_str!("../../../genesis/devnet.json")).unwrap();
    assert!(matches!(Chain::open(d.path(), &other), Err(ChainError::GenesisMismatch { .. })));
}

#[test]
fn blocks_are_published_to_the_blockstore_and_linked() {
    let g = dev_genesis();
    let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let producer = Chain::open(d1.path(), &g).unwrap();
    let follower = Chain::open(d2.path(), &g).unwrap();
    let key = dev_key();
    let t = tx(
        &key,
        g.config.chain_id,
        0,
        TxKind::Call(Address::repeat_byte(0xb0)),
        Bytes::new(),
        21_000,
    );
    let built = producer.build_block(vec![(t.clone(), key.address())], 10, Address::ZERO).unwrap();

    let r = producer.store().reader().unwrap();
    let genesis_root = r.envelope_root(0).unwrap().unwrap();
    assert_eq!(built.bundle.envelope.parent, Some(genesis_root));
    assert_eq!(r.envelope_root(1).unwrap(), Some(built.bundle.root));
    // The block can be rebuilt purely from the blockstore.
    let env = bolt_ipld::Envelope::decode(&r.ipld(&built.bundle.root).unwrap().unwrap()).unwrap();
    let decoded = bolt_ipld::decode_block(env, |c| r.ipld(c).unwrap()).unwrap();
    assert_eq!(decoded.header, built.header);
    assert_eq!(decoded.transactions, vec![t.clone()]);
    // Both nodes derive the same genesis envelope.
    assert_eq!(follower.store().reader().unwrap().envelope_root(0).unwrap(), Some(genesis_root));

    // A wrong announced root is rejected and nothing is written.
    let wrong = bolt_ipld::sha256_cid(bolt_ipld::DAG_CBOR, b"nope");
    assert!(follower.import_block_with_root(&built.header, vec![t.clone()], Some(wrong)).is_err());
    assert_eq!(follower.head().unwrap().number, 0);
    follower.import_block_with_root(&built.header, vec![t], Some(built.bundle.root)).unwrap();
    assert!(follower.store().reader().unwrap().ipld(&built.bundle.root).unwrap().is_some());
}

#[test]
fn pending_blocks_execute_on_pending_parents() {
    let g = dev_genesis();
    let (d1, d2) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let a = Chain::open(d1.path(), &g).unwrap();
    let b = Chain::open(d2.path(), &g).unwrap();
    let key = dev_key();
    let cid = g.config.chain_id;
    let bob = Address::repeat_byte(0xb0);
    let head = a.head().unwrap().hash_slow();

    // Two blocks built before either is final; the second spends state the first created.
    let t0 = tx(&key, cid, 0, TxKind::Call(bob), Bytes::new(), 21_000);
    let t1 = tx(&key, cid, 1, TxKind::Call(bob), Bytes::new(), 21_000);
    let qc0 = vec![0xaa; 10];
    let b1 = a
        .build_on(
            &head,
            vec![(t0.clone(), key.address())],
            10,
            Address::ZERO,
            1u64.to_be_bytes().to_vec().into(),
            qc0.clone(),
        )
        .unwrap();
    let qc1 = vec![0xbb; 10];
    let b2 = a
        .build_on(
            &b1.hash,
            vec![(t1.clone(), key.address())],
            16,
            Address::ZERO,
            2u64.to_be_bytes().to_vec().into(),
            qc1.clone(),
        )
        .unwrap();
    assert!(
        b1.rejected.is_empty() && b2.rejected.is_empty(),
        "{:?} {:?}",
        b1.rejected,
        b2.rejected
    );
    assert_eq!(b2.header.parent_hash, b1.hash);
    assert_eq!(b2.bundle.envelope.parent, Some(b1.bundle.root));
    assert_eq!(b2.header.parent_beacon_block_root, Some(keccak256(&qc1)));
    // Nothing is final yet.
    assert_eq!(a.head().unwrap().number, 0);
    assert_eq!(a.store().reader().unwrap().account(&bob).unwrap(), None);

    // Another node verifies both while pending (as a validator would before voting).
    b.verify_block(&b1.header, vec![t0], qc0).unwrap();
    b.verify_block(&b2.header, vec![t1], qc1.clone()).unwrap();
    // Tampering with the certificate is caught.
    let mut bad = b2.header.clone();
    bad.parent_beacon_block_root = Some(B256::ZERO);
    assert!(b.verify_block(&bad, vec![], qc1).is_err());

    for c in [&a, &b] {
        c.commit_pending(&b1.hash).unwrap();
        c.commit_pending(&b2.hash).unwrap();
        let r = c.store().reader().unwrap();
        assert_eq!(r.head().unwrap(), Some(2));
        assert_eq!(r.account(&bob).unwrap().unwrap().balance, U256::from(2_000u64));
        assert_eq!(bolt_store::full_state_root(&r).unwrap(), b2.header.state_root);
        assert_eq!(r.envelope_root(2).unwrap(), Some(b2.bundle.root));
    }
}

#[test]
fn competing_pending_blocks_resolve_on_commit() {
    let g = dev_genesis();
    let d = tempfile::tempdir().unwrap();
    let c = Chain::open(d.path(), &g).unwrap();
    let key = dev_key();
    let head = c.head().unwrap().hash_slow();
    let t = |to: u8| {
        tx(&key, g.config.chain_id, 0, TxKind::Call(Address::repeat_byte(to)), Bytes::new(), 21_000)
    };
    let x = c
        .build_on(&head, vec![(t(1), key.address())], 10, Address::ZERO, Bytes::new(), vec![])
        .unwrap();
    let y = c
        .build_on(&head, vec![(t(2), key.address())], 10, Address::ZERO, Bytes::new(), vec![])
        .unwrap();
    assert_ne!(x.hash, y.hash);
    c.commit_pending(&y.hash).unwrap();
    assert!(c.pending(&x.hash).is_none(), "the losing sibling is dropped");
    assert!(c.commit_pending(&x.hash).is_err());
    assert_eq!(c.head().unwrap().hash_slow(), y.hash);
}
