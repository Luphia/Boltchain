use super::*;
use bolt_primitives::bls::dev_key;

fn bls_set(n: usize) -> (Vec<BlsSecretKey>, Vec<BlsPublicKey>) {
    let sks: Vec<_> = (0..n as u32).map(dev_key).collect();
    let pks = sks.iter().map(|k| k.public_key()).collect();
    (sks, pks)
}

#[test]
fn bls_qc_and_tc_verify() {
    let (sks, pks) = bls_set(4);
    let set = ValidatorSet::equal(4);
    let g = B256::repeat_byte(9);
    let block = B256::repeat_byte(1);
    let msg = vote_msg(8017, 3, &block);
    let signers = [0u16, 1, 3];
    let sigs: Vec<_> = signers.iter().map(|i| sks[*i as usize].sign(&msg)).collect();
    let scheme = BlsScheme::new(pks.clone(), None);
    let qc: Qc<BlsScheme> = Qc {
        round: 3,
        block,
        height: 2,
        signers: bitmap::encode(&signers, 4),
        sig: scheme.aggregate(&sigs),
    };
    assert!(qc.verify(&scheme, &set, 8017, &g));
    // Two signers are not a quorum of four.
    let mut weak = qc.clone();
    weak.signers = bitmap::encode(&[0, 1], 4);
    assert!(!weak.verify(&scheme, &set, 8017, &g));
    // Wrong chain id.
    assert!(!qc.verify(&scheme, &set, 1, &g));

    let entries: Vec<_> = [0u16, 2, 3]
        .iter()
        .map(|i| (*i, 3u64, sks[*i as usize].sign(&timeout_msg(8017, 4, 3))))
        .collect();
    let tc = Tc { round: 4, high_qc: qc.clone(), entries };
    assert!(tc.verify(&scheme, &set, 8017, &g));
    let mut lying = tc.clone();
    lying.entries[0].1 = 2; // claims a lower QC than it signed
    assert!(!lying.verify(&scheme, &set, 8017, &g));
}

#[test]
fn messages_roundtrip_on_the_wire() {
    let (sks, pks) = bls_set(4);
    let scheme = BlsScheme::new(pks, Some(sks[1].clone()));
    let v: Message<BlsScheme> = Message::Vote(Vote {
        round: 5,
        block: B256::repeat_byte(3),
        height: 4,
        signer: 1,
        sig: scheme.sign(1, &vote_msg(8017, 5, &B256::repeat_byte(3))),
    });
    let bytes = encode(&v);
    assert_eq!(decode::<BlsScheme>(&bytes).map(|m| encode(&m)), Some(bytes.clone()));
    assert!(decode::<BlsScheme>(&[0xff, 0x00]).is_none());
}

#[test]
fn bitmap_roundtrip() {
    let s = [0u16, 7, 8, 511];
    assert_eq!(bitmap::decode(&bitmap::encode(&s, 512)), s.to_vec());
    assert_eq!(bitmap::encode(&s, 512).len(), 64);
}
