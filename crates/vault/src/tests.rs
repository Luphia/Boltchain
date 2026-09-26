use super::*;
use std::collections::HashMap;

fn store(s: &Sealed) -> HashMap<Cid, Vec<u8>> {
    s.blocks().map(|b| (b.cid, b.data.clone())).collect()
}

fn sample(n: usize) -> Vec<u8> {
    (0..n).map(|i| (i * 31 % 251) as u8).collect()
}

#[test]
fn round_trip_for_every_size_class() {
    let (req_sk, req_pk) = SecretKey::generate();
    let (exe_sk, exe_pk) = SecretKey::generate();
    for n in [0, 1, 4095, 16 * 1024, 70_000, 1_048_576 + 3, 3 * 1_048_576] {
        let data = sample(n);
        let s = seal(
            &data,
            "prompt.txt",
            "text/plain",
            &[req_pk.clone(), exe_pk.clone()],
            &Options::default(),
        )
        .unwrap();
        let blocks = store(&s);
        for sk in [&req_sk, &exe_sk] {
            let (m, out) = open(&s.envelope.data, sk, |c| blocks.get(c).cloned()).unwrap();
            assert_eq!(out, data, "size {n}");
            assert_eq!(
                (m.name.as_str(), m.mime.as_str(), m.size),
                ("prompt.txt", "text/plain", n as u64)
            );
        }
    }
}

#[test]
fn outsiders_see_only_ciphertext_and_a_size_bucket() {
    let (_, pk) = SecretKey::generate();
    let (other, _) = SecretKey::generate();
    let secret = b"the quick brown fox and a very private prompt".repeat(100);
    let s = seal(&secret, "private-name.txt", "text/plain", &[pk], &Options::default()).unwrap();
    let blocks = store(&s);
    assert!(matches!(
        open(&s.envelope.data, &other, |c| blocks.get(c).cloned()),
        Err(Error::NotRecipient)
    ));
    let needle = b"private";
    for b in s.blocks() {
        assert!(
            !b.data.windows(needle.len()).any(|w| w == needle),
            "plaintext leaked into a block"
        );
    }
    // 4,600 bytes pad to 4 chunks of 4 KiB (any size up to 16 KiB looks the same).
    assert_eq!(s.shards.len(), DEFAULT_K + DEFAULT_M);
    assert!(s.shards.iter().all(|b| b.data.len() == MIN_CHUNK + TAG));
}

#[test]
fn any_m_shards_per_stripe_may_be_lost() {
    let (sk, pk) = SecretKey::generate();
    let data = sample(2 * DEFAULT_K * MAX_CHUNK + 1000); // 3 stripes
    let s = seal(&data, "f", "application/octet-stream", &[pk], &Options::default()).unwrap();
    let env = parse_envelope(&s.envelope.data).unwrap();
    let key = open_key(&env, &sk).unwrap();
    let m = open_manifest(&env, &s.manifest.data, &key).unwrap();
    assert_eq!(m.stripes(), 3);
    let per = DEFAULT_K + DEFAULT_M;
    let mut shards: Vec<Option<Vec<u8>>> = s.shards.iter().map(|b| Some(b.data.clone())).collect();
    // Stripe 0 loses two data shards, stripe 1 a data and a parity shard, stripe 2 gets a
    // tampered shard plus a missing one.
    shards[0] = None;
    shards[2] = None;
    shards[per + 1] = None;
    shards[per + DEFAULT_K] = None;
    shards[2 * per + 3].as_mut().unwrap()[10] ^= 1;
    shards[2 * per] = None;
    assert_eq!(recover(&m, &key, &shards).unwrap(), data);
    // A third loss in one stripe is too many.
    shards[1] = None;
    assert!(matches!(
        recover(&m, &key, &shards),
        Err(Error::TooFewShards { stripe: 0, have: 3, need: 4 })
    ));
}

#[test]
fn a_recipient_can_be_added_without_re_uploading() {
    let (req_sk, req_pk) = SecretKey::generate();
    let (exe_sk, exe_pk) = SecretKey::generate();
    let data = sample(50_000);
    let s = seal(&data, "in.jsonl", "application/jsonl", &[req_pk], &Options::default()).unwrap();
    let blocks = store(&s);
    assert!(open(&s.envelope.data, &exe_sk, |c| blocks.get(c).cloned()).is_err());
    let env = parse_envelope(&s.envelope.data).unwrap();
    let key = open_key(&env, &req_sk).unwrap();
    let env2 = add_recipient(&env, &key, &exe_pk);
    let (_, out) = open(&env2.data, &exe_sk, |c| blocks.get(c).cloned()).unwrap();
    assert_eq!(out, data);
}

#[test]
fn tampering_is_detected() {
    let (sk, pk) = SecretKey::generate();
    let s = seal(&sample(10_000), "f", "x", &[pk], &Options { k: 4, m: 0 }).unwrap();
    let mut blocks = store(&s);
    // Without parity a changed shard cannot be repaired: it is refused, not returned.
    let c = s.shards[1].cid;
    blocks.get_mut(&c).unwrap()[0] ^= 0xff;
    assert!(open(&s.envelope.data, &sk, |c| blocks.get(c).cloned()).is_err());
    // A swapped manifest is refused.
    let env = parse_envelope(&s.envelope.data).unwrap();
    let key = open_key(&env, &sk).unwrap();
    let mut forged = s.manifest.data.clone();
    forged[0] ^= 1;
    assert!(open_manifest(&env, &forged, &key).is_err());
}

#[test]
fn keys_and_cids_round_trip() {
    let (sk, pk) = SecretKey::derive(&[7u8; 32]);
    let (sk2, pk2) = SecretKey::derive(&[7u8; 32]);
    assert_eq!(sk.to_bytes(), sk2.to_bytes());
    assert_eq!(pk, pk2);
    assert_eq!(SecretKey::from_bytes(&sk.to_bytes()).unwrap().public_key(), pk);
    assert_eq!(PublicKey::from_bytes(&pk.to_bytes()).unwrap(), pk);
    let c = Cid::of(b"hello");
    // Same string `ipfs add --raw-leaves --cid-version 1` prints for "hello".
    assert_eq!(c.to_string(), "bafkreibm6jg3ux5qumhcn2b3flc3tyu6dmlb4xa7u5bf44yegnrjhc4yeq");
    assert_eq!(c.to_string().parse::<Cid>().unwrap(), c);
}

#[test]
fn layout_buckets() {
    assert_eq!(layout(0, 4), (4096, 4));
    assert_eq!(layout(16 * 1024, 4), (16 * 1024, 4));
    assert_eq!(layout(16 * 1024 - 1, 4), (4096, 4));
    assert_eq!(layout(1 << 20, 4), (MAX_CHUNK, 4));
    assert_eq!(layout((1 << 20) + 1, 4), (MAX_CHUNK, 8));
}
