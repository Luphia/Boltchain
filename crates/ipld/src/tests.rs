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
