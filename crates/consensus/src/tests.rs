use super::*;
use bolt_primitives::bls::dev_key;

fn bls_set(n: usize) -> (Vec<BlsSecretKey>, Vec<BlsPublicKey>) {
    let sks: Vec<_> = (0..n as u32).map(dev_key).collect();
    let pks = sks.iter().map(|k| k.public_key()).collect();
    (sks, pks)
}

fn qc_for(
    sks: &[BlsSecretKey],
    scheme: &BlsScheme,
    epoch: u64,
    round: u64,
    block: B256,
    height: u64,
    signers: &[u16],
) -> Qc<BlsScheme> {
    let msg = vote_msg(8017, epoch, round, &block);
    let sigs: Vec<_> = signers.iter().map(|i| sks[*i as usize].sign(&msg)).collect();
    Qc {
        epoch,
        round,
        block,
        height,
        signers: bitmap::encode(signers, sks.len()),
        sig: scheme.aggregate(&sigs),
    }
}

#[test]
fn bls_qc_and_tc_verify() {
    let (sks, pks) = bls_set(4);
    let set = ValidatorSet::equal(4);
    let g = B256::repeat_byte(9);
    let block = B256::repeat_byte(1);
    let scheme = BlsScheme::new(pks.clone(), &[]);
    let qc = qc_for(&sks, &scheme, 2, 3, block, 2, &[0, 1, 3]);
    assert!(qc.verify(&scheme, &set, 8017, 2, &g));
    // Other epoch: rounds restart each epoch, so signatures are epoch-bound.
    assert!(!qc.verify(&scheme, &set, 8017, 3, &g));
    let mut relabeled = qc.clone();
    relabeled.epoch = 3;
    assert!(!relabeled.verify(&scheme, &set, 8017, 3, &g));
    // Two signers are not a quorum of four.
    let mut weak = qc.clone();
    weak.signers = bitmap::encode(&[0, 1], 4);
    assert!(!weak.verify(&scheme, &set, 8017, 2, &g));
    // Wrong chain id.
    assert!(!qc.verify(&scheme, &set, 1, 2, &g));
    // Anchor certificates only for the anchor.
    assert!(Qc::<BlsScheme>::anchor(2, g, 10).verify(&scheme, &set, 8017, 2, &g));
    assert!(!Qc::<BlsScheme>::anchor(2, block, 10).verify(&scheme, &set, 8017, 2, &g));

    let entries: Vec<_> = [0u16, 2, 3]
        .iter()
        .map(|i| (*i, 3u64, sks[*i as usize].sign(&timeout_msg(8017, 2, 4, 3))))
        .collect();
    let tc = Tc { epoch: 2, round: 4, high_qc: qc.clone(), entries };
    assert!(tc.verify(&scheme, &set, 8017, 2, &g));
    let mut lying = tc.clone();
    lying.entries[0].1 = 2; // claims a lower QC than it signed
    assert!(!lying.verify(&scheme, &set, 8017, 2, &g));
}

#[test]
fn weighted_seats_set_quorum_and_leaders() {
    // member 0 holds 3 seats, members 1 and 2 one each
    let set = ValidatorSet::from_seats(3, vec![0, 1, 0, 2, 0]);
    assert_eq!(set.weights, vec![3, 1, 1]);
    assert!(set.is_quorum(set.weight_of(&[0, 1])), "4 of 5 seats");
    assert!(!set.is_quorum(set.weight_of(&[1, 2])));
    assert!(!set.is_quorum(set.weight_of(&[0])), "3 of 5 seats is not > 2/3");
    let leaders: Vec<_> = (0..5).map(|r| set.leader(r)).collect();
    assert_eq!(leaders, vec![0, 1, 0, 2, 0]);
}

#[test]
fn commit_proofs_real_and_nil_children() {
    let (sks, pks) = bls_set(4);
    let set = ValidatorSet::equal(4);
    let anchor = B256::repeat_byte(9);
    let scheme = BlsScheme::new(pks, &[]);
    // Real child: header binds it to the parent.
    let parent = B256::repeat_byte(5);
    let child = alloy_consensus::Header { parent_hash: parent, number: 8, ..Default::default() };
    let rlp = alloy_rlp::encode(&child);
    let child_hash = keccak256(&rlp);
    let proof = CommitProof {
        qc: qc_for(&sks, &scheme, 1, 6, parent, 7, &[0, 1, 2]),
        child_qc: qc_for(&sks, &scheme, 1, 7, child_hash, 8, &[1, 2, 3]),
        child_header: rlp.clone(),
        nil_rounds: vec![],
        committed: parent,
        committed_height: 7,
    };
    assert_eq!(proof.verify(&scheme, &set, 8017, &anchor), Some(parent));
    let mut gap = proof.clone();
    gap.child_qc = qc_for(&sks, &scheme, 1, 8, child_hash, 8, &[1, 2, 3]);
    assert_eq!(gap.verify(&scheme, &set, 8017, &anchor), None, "rounds must be consecutive");

    // Epoch end at height 10 (X): nil n1 (round 12) on X, nil n2 (round 13) on n1.
    let x = B256::repeat_byte(0x10);
    let n1 = nil_hash(1, &x, 12);
    let n2 = nil_hash(1, &n1, 13);
    let nil_proof = CommitProof {
        qc: qc_for(&sks, &scheme, 1, 12, n1, 11, &[0, 1, 2]),
        child_qc: qc_for(&sks, &scheme, 1, 13, n2, 12, &[0, 1, 3]),
        child_header: vec![],
        nil_rounds: vec![12, 13],
        committed: x,
        committed_height: 10,
    };
    assert_eq!(nil_proof.verify(&scheme, &set, 8017, &anchor), Some(x));
    let mut other = nil_proof.clone();
    other.committed = B256::repeat_byte(0x11);
    assert_eq!(
        other.verify(&scheme, &set, 8017, &anchor),
        None,
        "nil chain must hang off the committed block"
    );
    // X directly certified, nil child in the next round.
    let n = nil_hash(1, &x, 11);
    let direct = CommitProof {
        qc: qc_for(&sks, &scheme, 1, 10, x, 10, &[0, 1, 2]),
        child_qc: qc_for(&sks, &scheme, 1, 11, n, 11, &[0, 1, 2]),
        child_header: vec![],
        nil_rounds: vec![11],
        committed: x,
        committed_height: 10,
    };
    assert_eq!(direct.verify(&scheme, &set, 8017, &anchor), Some(x));

    // Certificates round-trip and expose their votes.
    let cert = Cert::Epoch(direct.clone());
    let bytes = encode_cert(&cert);
    assert_eq!(decode_cert::<BlsScheme>(&bytes).map(|c| encode_cert(&c)), Some(bytes.clone()));
    assert_eq!(cert_votes(&bytes), (1, 10, direct.qc.signers.clone()));
    assert!(encode_cert(&Cert::Qc(Qc::<BlsScheme>::genesis(anchor))).is_empty());
    assert_eq!(cert_votes(&[]), (0, 0, vec![]));
}

#[test]
fn messages_roundtrip_on_the_wire() {
    let (sks, pks) = bls_set(4);
    let scheme = BlsScheme::new(pks, &[sks[1].clone(), sks[3].clone()]);
    assert_eq!(scheme.signers(), vec![1, 3]);
    let v: Message<BlsScheme> = Message::Vote(Vote {
        epoch: 0,
        round: 5,
        block: B256::repeat_byte(3),
        height: 4,
        signer: 1,
        sig: scheme.sign(1, &vote_msg(8017, 0, 5, &B256::repeat_byte(3))),
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

/// The next leader checks a quorum of votes as one aggregate; a forged vote costs it the
/// fallback to per-vote checks but neither blocks the QC nor gets into it, even when the forgery
/// arrives before the genuine vote of the same signer.
#[test]
fn votes_are_checked_as_one_aggregate_and_forgeries_are_dropped() {
    let (sks, pks) = bls_set(4);
    let validators = ValidatorSet::equal(4);
    let genesis = B256::repeat_byte(1);
    let leader2 = validators.leader(2);
    let scheme = BlsScheme::new(pks.clone(), &sks[leader2 as usize..=leader2 as usize]);
    let cfg = Config::genesis(8017, validators, Some(leader2), genesis, 1000);
    let mut engine = Engine::new(cfg, scheme);
    let _ = engine.start();
    let block = B256::repeat_byte(9);
    let msg = vote_msg(8017, 0, 1, &block);
    let vote = |signer: u16, sig| {
        Message::Vote(Vote { epoch: 0, round: 1, block, height: 1, signer, sig })
    };
    let others: Vec<u16> = (0..4u16).filter(|i| *i != leader2).collect();
    // A forgery for the first signer arrives first (signed with the wrong key).
    let forged = sks[others[1] as usize].sign(&msg);
    let _ = engine.on_message(vote(others[0], forged));
    let _ = engine.on_message(vote(others[1], sks[others[1] as usize].sign(&msg)));
    let _ = engine.on_message(vote(others[2], sks[others[2] as usize].sign(&msg)));
    assert_eq!(engine.high_qc().round, 0, "the forged vote kept this from being a quorum");
    // The genuine vote of the same signer still counts.
    let _ = engine.on_message(vote(others[0], sks[others[0] as usize].sign(&msg)));
    let qc = engine.high_qc().clone();
    assert_eq!((qc.round, qc.block), (1, block));
    let signers = bitmap::decode(&qc.signers);
    assert_eq!(signers, others);
    let check = BlsScheme::new(pks, &[]);
    assert!(check.verify_aggregate(&signers, &msg, qc.sig.as_ref().unwrap()));
}

/// Runs a single engine that holds every seat until `done` or `steps` actions; returns whether
/// it asked to fetch a block.
fn drive(
    engine: &mut Engine<BlsScheme>,
    mut queue: std::collections::VecDeque<Action<BlsScheme>>,
    done: impl Fn(&Engine<BlsScheme>) -> bool,
) -> bool {
    let mut fetched = false;
    for _ in 0..10_000 {
        if done(engine) {
            break;
        }
        let Some(a) = queue.pop_front() else { break };
        let out = match a {
            Action::Broadcast(m) | Action::SendTo(_, m) => engine.on_message(m),
            Action::Validate(p) => engine.on_validated(p.block.hash, true),
            Action::Propose { round, qc, tc } => {
                let hash =
                    alloy_primitives::keccak256([&round.to_be_bytes()[..], &qc.block[..]].concat());
                let block = BlockInfo { hash, parent: qc.block, round, height: qc.height + 1 };
                engine.on_proposed(block, qc, tc, Vec::new())
            }
            Action::FetchBlock(_) => {
                fetched = true;
                Vec::new()
            }
            _ => Vec::new(),
        };
        queue.extend(out);
    }
    fetched
}

/// A restarted validator keeps committing: the blocks above its last committed one are part of
/// its persisted state, so a new certificate can be walked back to it. (Phase B checkpoint rounds
/// are not in mined headers, so a node that forgot them could not fetch them back.)
#[test]
fn resumed_engine_commits_without_refetching_uncommitted_blocks() {
    let (sks, pks) = bls_set(4);
    let genesis = B256::repeat_byte(1);
    let cfg = || {
        let mut c = Config::genesis(8017, ValidatorSet::equal(4), None, genesis, 1000);
        c.me = vec![0, 1, 2, 3];
        c
    };
    let mut e = Engine::new(cfg(), BlsScheme::new(pks.clone(), &sks));
    let start = e.start().into();
    drive(&mut e, start, |e| e.committed().height >= 5);
    let saved = e.persisted();
    assert!(!saved.blocks.is_empty(), "the certified block above the committed one is kept");
    let json = serde_json::to_vec(&saved).unwrap();
    let restored: Persisted<BlsScheme> = serde_json::from_slice(&json).unwrap();

    let committed = saved.committed.height;
    let mut resumed = Engine::resume(cfg(), BlsScheme::new(pks.clone(), &sks), restored);
    let start = resumed.start().into();
    let fetched = drive(&mut resumed, start, |e| e.committed().height > committed + 2);
    assert!(!fetched, "nothing had to be fetched");
    assert!(resumed.committed().height > committed + 2);

    // State written by the previous release (no `blocks`): the same walk needs a fetch.
    let mut old = saved.clone();
    old.blocks.clear();
    let mut forgetful = Engine::resume(cfg(), BlsScheme::new(pks, &sks), old);
    let start = forgetful.start().into();
    assert!(drive(&mut forgetful, start, |e| e.committed().height > committed + 2));
}
