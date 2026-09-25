use super::*;
use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_network::TxSignerSync;
use alloy_primitives::{Address, TxKind, U256};
use alloy_signer_local::PrivateKeySigner;
use std::collections::HashMap;

fn txs(n: usize, data_len: usize) -> Vec<TxEnvelope> {
    let k = PrivateKeySigner::random();
    (0..n)
        .map(|i| {
            let mut t = TxEip1559 {
                chain_id: 1337,
                nonce: i as u64,
                gas_limit: 1_000_000,
                max_fee_per_gas: 1,
                to: TxKind::Call(Address::repeat_byte(1)),
                value: U256::from(i),
                input: vec![0xab; data_len].into(),
                ..Default::default()
            };
            let sig = k.sign_transaction_sync(&mut t).unwrap();
            t.into_signed(sig).into()
        })
        .collect()
}

fn header(number: u64) -> Header {
    Header { number, gas_limit: 30_000_000, timestamp: 1000 + number, ..Default::default() }
}

#[test]
fn header_cid_digest_is_block_hash() {
    let h = header(7);
    let b = bundle(&h, &[], None, vec![]);
    assert_eq!(b.envelope.block_hash(), Some(h.hash_slow()));
    assert_eq!(b.envelope.header.codec(), ETH_BLOCK);
    // CIDv1 base32 string starts with 'b'; eth-block + keccak-256 is the standard encoding.
    assert!(b.envelope.header.to_string().starts_with("bagiacgza"), "{}", b.envelope.header);
}

#[test]
fn roundtrip_small_and_large_bodies() {
    for (n, len, expect_chunks) in [(0usize, 0usize, 0usize), (3, 100, 1), (40, 60_000, 3)] {
        let t = txs(n, len);
        let h = header(1);
        let parent = Some(sha256_cid(DAG_CBOR, b"parent"));
        let b = bundle(&h, &t, parent, vec![1, 2, 3]);
        assert_eq!(b.envelope.chunks.len(), expect_chunks, "n={n}");
        for (cid, data) in &b.blocks {
            verify(cid, data).unwrap();
            assert!(data.len() <= MAX_IPLD_BLOCK_BYTES);
        }
        let store: HashMap<Cid, Vec<u8>> = b.blocks.iter().cloned().collect();
        let env = Envelope::decode(&store[&b.root]).unwrap();
        assert_eq!(env, b.envelope);
        let d = decode_block(env, |c| store.get(c).cloned()).unwrap();
        assert_eq!(d.header, h);
        assert_eq!(d.transactions, t);
        // Deterministic: same input, same root.
        assert_eq!(bundle(&h, &t, parent, vec![1, 2, 3]).root, b.root);
    }
}

#[test]
fn tampering_is_detected() {
    let t = txs(2, 10);
    let b = bundle(&header(1), &t, None, vec![]);
    let mut store: HashMap<Cid, Vec<u8>> = b.blocks.iter().cloned().collect();
    let chunk = b.envelope.chunks[0];
    store.get_mut(&chunk).unwrap()[5] ^= 1;
    assert_eq!(
        decode_block(b.envelope.clone(), |c| store.get(c).cloned()),
        Err(IpldError::HashMismatch(chunk))
    );
    store.remove(&chunk);
    assert_eq!(
        decode_block(b.envelope.clone(), |c| store.get(c).cloned()),
        Err(IpldError::Missing(chunk))
    );
    // Unsupported hash/codec combinations are rejected outright.
    let odd = Cid::new_v1(RAW, mh(KECCAK_256, &[0u8; 32]));
    assert_eq!(verify(&odd, b"x"), Err(IpldError::Unsupported(odd)));
}

#[test]
fn car_roundtrip() {
    let b = bundle(&header(3), &txs(5, 1000), None, vec![]);
    let car = car::write(&[b.root], &b.blocks);
    let (roots, blocks) = car::read(&car).unwrap();
    assert_eq!(roots, vec![b.root]);
    assert_eq!(blocks, b.blocks);
    let mut bad = car.clone();
    let last = bad.len() - 1;
    bad[last] ^= 0xff;
    assert!(car::read(&bad).is_err());
}

#[test]
fn epoch_index_groups_envelopes() {
    use crate::history::*;
    let envs: Vec<Cid> = (0..2500u32).map(|i| sha256_cid(DAG_CBOR, &i.to_be_bytes())).collect();
    let (root, blocks) = epoch_index(3, 7201, &envs);
    assert_eq!(blocks.len(), 4, "3 groups + root");
    for (c, b) in &blocks {
        verify(c, b).unwrap();
    }
    let idx = EpochIndex::decode(&blocks.last().unwrap().1).unwrap();
    assert_eq!(blocks.last().unwrap().0, root);
    assert_eq!((idx.epoch, idx.first, idx.count, idx.groups.len()), (3, 7201, 2500, 3));
    let g1 = decode_group(&blocks[1].1).unwrap();
    assert_eq!(g1.len(), EPOCH_GROUP);
    assert_eq!(g1[0], envs[EPOCH_GROUP]);
    assert_eq!(epoch_index(3, 7201, &envs).0, root, "deterministic");
}

#[test]
fn snapshot_chunks_are_deterministic_and_split_big_storage() {
    use crate::history::*;
    let build = || {
        let mut b = SnapshotBuilder::default();
        let mut blocks = Vec::new();
        for i in 0..50u8 {
            let slots: Vec<([u8; 32], [u8; 32])> = if i == 7 {
                // an account with more storage than one chunk holds
                (0..20_000u32)
                    .map(|k| {
                        let mut s = [0u8; 32];
                        s[28..].copy_from_slice(&k.to_be_bytes());
                        (s, [i; 32])
                    })
                    .collect()
            } else {
                vec![([1; 32], [i; 32])]
            };
            blocks.extend(b.account([i; 20], i as u64, [i; 32], [0xc0; 32], slots));
            blocks.extend(b.code(&[i; 100]));
        }
        blocks.extend(b.finish());
        (b.accounts.clone(), b.code_chunks.clone(), blocks)
    };
    let (a1, c1, blocks) = build();
    let (a2, c2, _) = build();
    assert_eq!((a1.clone(), c1.clone()), (a2, c2), "deterministic");
    assert!(a1.len() >= 3, "big account split: {} chunks", a1.len());
    let mut entries = Vec::new();
    for (cid, bytes) in &blocks {
        verify(cid, bytes).unwrap();
        assert!(bytes.len() <= bolt_primitives::params::MAX_IPLD_BLOCK_BYTES);
        if a1.contains(cid) {
            entries.extend(decode_accounts(bytes).unwrap());
        }
    }
    let slots_of_7: usize =
        entries.iter().filter(|e| e.a.as_ref() == [7u8; 20]).map(|e| e.s.len()).sum();
    assert_eq!(slots_of_7, 20_000);
    assert_eq!(entries.iter().filter(|e| !e.x).count(), 50);
    let codes: Vec<_> = blocks
        .iter()
        .filter(|(c, _)| c1.contains(c))
        .flat_map(|(_, b)| decode_codes(b).unwrap())
        .collect();
    assert_eq!(codes.len(), 50);
}

#[test]
fn envelopes_without_audits_encode_as_before() {
    let e = Envelope {
        v: ENVELOPE_VERSION,
        height: 1,
        header: header_cid(B256::repeat_byte(1)),
        parent: None,
        chunks: vec![],
        qc: vec![],
        audits: vec![],
    };
    let bytes = e.encode();
    #[derive(serde::Serialize)]
    struct Old {
        v: u8,
        height: u64,
        header: Cid,
        parent: Option<Cid>,
        chunks: Vec<Cid>,
        #[serde(with = "serde_bytes")]
        qc: Vec<u8>,
    }
    let old = serde_ipld_dagcbor::to_vec(&Old {
        v: e.v,
        height: 1,
        header: e.header,
        parent: None,
        chunks: vec![],
        qc: vec![],
    })
    .unwrap();
    assert_eq!(bytes, old);
    let with = Envelope { audits: vec![serde_bytes::ByteBuf::from(vec![1, 2])], ..e };
    assert_eq!(Envelope::decode(&with.encode()).unwrap(), with);
}
