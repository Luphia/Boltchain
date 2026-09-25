//! Mined blocks: sealing, difficulty, rewards, fork choice, bounded reorganisations and the
//! switch to PoS (ADR 0007).

use super::*;
use crate::pow::MinedOutcome;
use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_network::TxSignerSync;
use alloy_primitives::TxKind;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall;
use bolt_primitives::{
    bls::{dev_key, pubkey_point, signature_point},
    genesis::PowAlgorithm,
    params::{SUPPLY_CAP_WEI, WEI_PER_BOLT},
};
use bolt_system::{abi::*, addresses::*, queries};
use std::sync::atomic::AtomicBool;

const L: u64 = 4;

/// Dev PoW chain: keccak seal, easy difficulty, 4-block epochs, PoS after 2 epochs with 2
/// stakers holding 128 BOLT.
fn genesis() -> Genesis {
    let mut g = Genesis::from_json(include_str!("../../../genesis/pow-dev.json")).unwrap();
    g.config.pow.algorithm = PowAlgorithm::Keccak;
    g.config.pow.initial_difficulty = U256::from(64);
    g.config.pow.block_seconds = 12;
    g.config.pow.half_life_seconds = 120;
    g.config.epoch_slots = L;
    g.config.committee_size = 4;
    // T1 = T2: phase B never runs here (phase B has its own tests).
    g.config.checkpoint_min_stakers = Some(2);
    g.config.checkpoint_min_stake_bolt = Some(128);
    g.config.pos_min_stakers = Some(2);
    g.config.pos_min_stake_bolt = Some(128);
    g.validate().unwrap();
    g
}

fn open(g: &Genesis) -> (tempfile::TempDir, Chain) {
    let d = tempfile::tempdir().unwrap();
    let c = Chain::open(d.path(), g).unwrap();
    (d, c)
}

fn signer() -> PrivateKeySigner {
    "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".parse().unwrap()
}

fn bolt(n: u64) -> U256 {
    U256::from(n) * U256::from(WEI_PER_BOLT)
}

fn call_tx(nonce: u64, to: Address, value: U256, data: Vec<u8>) -> TxEnvelope {
    let mut t = TxEip1559 {
        chain_id: 1338,
        nonce,
        gas_limit: 3_000_000,
        max_fee_per_gas: 2_000_000_000,
        max_priority_fee_per_gas: 1_000_000,
        to: TxKind::Call(to),
        value,
        input: data.into(),
        ..Default::default()
    };
    let sig = signer().sign_transaction_sync(&mut t).unwrap();
    t.into_signed(sig).into()
}

fn register_tx(nonce: u64, key: u32) -> TxEnvelope {
    register_tx_with(nonce, key, bolt(100))
}

fn register_tx_with(nonce: u64, key: u32, stake: U256) -> TxEnvelope {
    let sk = dev_key(key);
    let pk = sk.public_key();
    let data = IStakingManager::registerCall {
        pubkey: Bytes::copy_from_slice(pk.as_slice()),
        pubkeyPoint: Bytes::copy_from_slice(&pubkey_point(&pk).unwrap()),
        pop: Bytes::copy_from_slice(&signature_point(&sk.proof_of_possession()).unwrap()),
        feeRecipient: Address::repeat_byte(0x50 + key as u8),
    }
    .abi_encode();
    call_tx(nonce, STAKING, stake, data)
}

/// Mines the next block on `chain`'s head, `spacing` seconds after it.
fn mine(
    chain: &Chain,
    txs: Vec<TxEnvelope>,
    miner: Address,
    spacing: u64,
) -> (Header, MinedOutcome) {
    let head = chain.head().unwrap();
    let cands: Vec<_> = txs.into_iter().map(|t| (t.clone(), t.recover_signer().unwrap())).collect();
    let n = cands.len();
    let t = chain
        .build_template(
            cands,
            head.timestamp + spacing,
            miner,
            Bytes::from_static(b"test miner"),
            vec![],
        )
        .unwrap();
    assert_eq!(t.included.len(), n, "{:?}", t.rejected);
    let key = chain.seal_key(&head.hash_slow(), head.number + 1).unwrap();
    let stop = AtomicBool::new(false);
    let seal = bolt_pow::seal_hash(&t.header);
    let (nonce, _) = chain
        .pow()
        .search(&key, &seal, t.header.difficulty, 0, 1, u64::MAX, &stop)
        .unwrap()
        .unwrap();
    chain.seal_and_import(&t, nonce).unwrap()
}

fn body(chain: &Chain, n: u64) -> Vec<TxEnvelope> {
    chain.store().reader().unwrap().block(n).unwrap().unwrap().transactions
}

fn view<C: SolCall>(chain: &Chain, to: Address, c: C) -> C::Return {
    let r = chain.store().reader().unwrap();
    queries::call(&StateView::latest(&r), 1338, to, c).unwrap()
}

fn balance(chain: &Chain, a: Address) -> U256 {
    let r = chain.store().reader().unwrap();
    revm::DatabaseRef::basic_ref(&StateView::latest(&r), a)
        .unwrap()
        .map(|i| i.balance)
        .unwrap_or_default()
}

#[test]
fn mined_blocks_pay_the_miner_and_follow_asert() {
    let g = genesis();
    let (_d, chain) = open(&g);
    let (_d2, follower) = open(&g);
    let miner = Address::repeat_byte(0x33);
    let supply0 = view(&chain, REWARDS, IRewardDistributor::supplyCall {});
    let (h1, o) = mine(&chain, vec![], miner, 12);
    assert_eq!(o, MinedOutcome::Extended);
    assert_eq!(h1.difficulty, U256::from(64), "initial difficulty");
    let reward = bolt_pow::block_reward(SUPPLY_CAP_WEI - supply0, 8_000);
    assert!(reward > bolt(30) && reward < bolt(300), "{reward}");
    assert_eq!(balance(&chain, miner), reward);
    assert_eq!(view(&chain, REWARDS, IRewardDistributor::supplyCall {}), supply0 + reward);
    // Fast blocks raise the difficulty, slow ones lower it.
    let (h2, _) = mine(&chain, vec![], miner, 12);
    assert_eq!(h2.difficulty, U256::from(64), "block 2 also uses the initial difficulty");
    let (h3, _) = mine(&chain, vec![], miner, 2);
    assert_eq!(h3.difficulty, U256::from(64), "block 2 came on schedule");
    let (h4, _) = mine(&chain, vec![], miner, 12);
    assert!(h4.difficulty > h3.difficulty, "block 3 was fast");
    let (h5, _) = mine(&chain, vec![], miner, 12);
    let (h6, _) = mine(&chain, vec![], miner, 500);
    let (h7, _) = mine(&chain, vec![], miner, 12);
    assert!(h7.difficulty < h6.difficulty, "block 6 was slow");
    assert_eq!(
        chain.head_total_difficulty().unwrap(),
        [h1, h2, h3, h4, h5.clone(), h6, h7.clone()].iter().map(|h| h.difficulty).sum::<U256>()
    );

    // A follower verifies each seal and replays the blocks.
    for n in 1..=7 {
        let h = chain.store().reader().unwrap().header(n).unwrap().unwrap();
        assert_eq!(
            follower.import_mined(&h, body(&chain, n), None).unwrap(),
            MinedOutcome::Extended
        );
    }
    assert_eq!(follower.head().unwrap(), h7);
    assert_eq!(follower.import_mined(&h7, vec![], None).unwrap(), MinedOutcome::Known);

    // Bad seal, wrong difficulty, PoS-style block: all rejected before execution.
    let (_d3, other) = open(&g);
    let h1 = chain.store().reader().unwrap().header(1).unwrap().unwrap();
    let mut bad = h1.clone();
    bad.nonce = alloy_primitives::B64::from(u64::from_be_bytes(bad.nonce.0).wrapping_add(1));
    assert!(matches!(other.import_mined(&bad, vec![], None), Err(ChainError::InvalidBlock(_))));
    let mut wrong = h1.clone();
    wrong.difficulty = U256::from(1);
    assert!(matches!(other.import_mined(&wrong, vec![], None), Err(ChainError::InvalidBlock(_))));
    let mut pos = h1.clone();
    pos.difficulty = U256::ZERO;
    assert!(matches!(other.import_mined(&pos, vec![], None), Err(ChainError::InvalidBlock(_))));
    let _ = h5;
}

#[test]
fn heavier_branch_wins_and_state_is_rewound() {
    let g = genesis();
    let (_da, a) = open(&g);
    let (_db, b) = open(&g);
    let (miner_a, miner_b) = (Address::repeat_byte(0xaa), Address::repeat_byte(0xbb));
    // Common prefix: blocks 1-2 on both.
    for _ in 0..2 {
        let (h, _) = mine(&a, vec![], miner_a, 12);
        b.import_mined(&h, vec![], None).unwrap();
    }
    // A mines 2 blocks with a transfer; B mines 3 (more work) without it.
    let transfer = call_tx(0, Address::repeat_byte(0xcc), bolt(5), vec![]);
    mine(&a, vec![transfer.clone()], miner_a, 12);
    mine(&a, vec![], miner_a, 12);
    assert_eq!(balance(&a, Address::repeat_byte(0xcc)), bolt(5));
    let mut b_blocks = Vec::new();
    for _ in 0..3 {
        let (h, _) = mine(&b, vec![], miner_b, 12);
        b_blocks.push(h);
    }
    let td_a = a.head_total_difficulty().unwrap();
    // Feed B's branch to A: the first two stay on the side, the third tips the balance.
    assert_eq!(a.import_mined(&b_blocks[0], vec![], None).unwrap(), MinedOutcome::Side);
    let a_tip = a.head().unwrap().hash_slow();
    let second = a.import_mined(&b_blocks[1], vec![], None).unwrap();
    // Equal work: the lower hash wins (the same choice on every node).
    if b_blocks[1].hash_slow() < a_tip {
        assert_eq!(second, MinedOutcome::Reorged { depth: 2 });
        assert_eq!(a.import_mined(&b_blocks[2], vec![], None).unwrap(), MinedOutcome::Extended);
    } else {
        assert_eq!(second, MinedOutcome::Side);
        assert_eq!(
            a.import_mined(&b_blocks[2], vec![], None).unwrap(),
            MinedOutcome::Reorged { depth: 2 }
        );
    }
    assert_eq!(a.head().unwrap(), b_blocks[2].clone());
    assert!(a.head_total_difficulty().unwrap() > td_a);
    // The transfer is undone; miner A kept only the common-prefix rewards.
    assert_eq!(balance(&a, Address::repeat_byte(0xcc)), U256::ZERO);
    assert_eq!(balance(&a, miner_b), balance(&b, miner_b));
    let r = a.store().reader().unwrap();
    assert_eq!(r.header(5).unwrap().unwrap(), b_blocks[2]);
    assert_eq!(bolt_store::full_state_root(&r).unwrap(), b_blocks[2].state_root);
    drop(r);
    // The old branch can come back if it grows heavier.
    let (h, o) = mine(&b, vec![], miner_b, 12);
    assert_eq!(o, MinedOutcome::Extended);
    assert_eq!(a.import_mined(&h, vec![], None).unwrap(), MinedOutcome::Extended);
}

#[test]
fn reorganisations_deeper_than_128_blocks_are_refused() {
    let g = genesis();
    let (_da, a) = open(&g);
    let (_db, b) = open(&g);
    // Past the first epochs nobody stakes, so the chain stays mined.
    for _ in 0..130 {
        mine(&a, vec![], Address::repeat_byte(0xaa), 12);
    }
    let mut branch = Vec::new();
    for _ in 0..135 {
        let (h, _) = mine(&b, vec![], Address::repeat_byte(0xbb), 10);
        branch.push(h);
    }
    assert!(b.head_total_difficulty().unwrap() > a.head_total_difficulty().unwrap());
    let mut last = None;
    for h in &branch {
        last = Some(a.import_mined(h, vec![], None));
        if matches!(last, Some(Err(_))) {
            break;
        }
    }
    assert!(matches!(last, Some(Err(ChainError::DeepReorg(_)))), "{last:?}");
    assert_eq!(a.head().unwrap().number, 130, "head unchanged");
}

#[test]
fn stake_thresholds_switch_the_chain_to_pos() {
    let g = genesis();
    let (_d, chain) = open(&g);
    let (_f, follower) = open(&g);
    let miner = Address::repeat_byte(0x33);
    let mut blocks: Vec<Header> = Vec::new();
    let mut push = |chain: &Chain, h: Header| {
        blocks.push(h);
        let _ = chain;
    };
    // Block 1 (epoch 0): two validators with 100 BOLT each (thresholds: 2 stakers, 128 BOLT).
    let (h, _) = mine(&chain, vec![register_tx(0, 20), register_tx(1, 21)], miner, 12);
    push(&chain, h);
    assert_eq!(view(&chain, STAKING, IStakingManager::countCall {}), 2);
    // Epoch starts at blocks 5 and 9 see the thresholds met; the second one completes the
    // 2-epoch streak and schedules PoS for epoch 2 + 2 = 4 (blocks 17..).
    while chain.head().unwrap().number < 9 {
        let (h, _) = mine(&chain, vec![], miner, 12);
        push(&chain, h);
    }
    let phase = chain.phase().unwrap();
    assert_eq!(phase.pos_epoch, Some(4));
    assert_eq!(phase.terminal_height(chain.rules()), Some(16));
    // A whale stakes 50x more right before the first committee is drawn (block 11, epoch 2)...
    while chain.head().unwrap().number < 10 {
        let (h, _) = mine(&chain, vec![], miner, 12);
        push(&chain, h);
    }
    let (h, _) = mine(&chain, vec![register_tx_with(2, 22, bolt(5_000))], miner, 12);
    push(&chain, h);
    // ...but the committee of epoch 4, drawn at block 13 (start of epoch 3), only counts stake at
    // least 2 epochs old: the whale gets no seat.
    while chain.head().unwrap().number < 16 {
        let (h, _) = mine(&chain, vec![], miner, 12);
        push(&chain, h);
    }
    let c = view(&chain, CONSENSUS, IConsensusRegistry::committeeCall { epoch: 4 });
    assert_eq!(c.weights.iter().map(|w| *w as u32).sum::<u32>(), 4);
    assert!(c.ids.iter().all(|id| [1, 2].contains(id)), "{:?}", c.ids);
    assert!(view(&chain, CONSENSUS, IConsensusRegistry::committeeCall { epoch: 3 }).ids.is_empty());

    // Block 17 must come from the PoS committee: mining it fails...
    let head = chain.head().unwrap();
    assert!(matches!(
        chain.build_template(vec![], head.timestamp + 12, miner, Bytes::new(), vec![]),
        Err(ChainError::InvalidBlock(_))
    ));
    // ...a PoS block is accepted (certificates are the node's business)...
    let pos = chain
        .build_on(
            &head.hash_slow(),
            vec![],
            head.timestamp + 6,
            Address::repeat_byte(0x51),
            Bytes::new(),
            vec![],
        )
        .unwrap();
    assert!(pos.header.difficulty.is_zero());
    chain.commit_pending(&pos.hash).unwrap();
    // Later committees count all stake: the whale now holds most seats.
    let c5 = view(&chain, CONSENSUS, IConsensusRegistry::committeeCall { epoch: 5 });
    let whale = c5.ids.iter().position(|id| *id == 3).map(|i| c5.weights[i]).unwrap_or(0);
    assert!(whale >= 3, "whale seats in epoch 5: {whale} of 4");
    // ...and a mined block at that height is invalid everywhere.
    for (n, h) in blocks.iter().enumerate() {
        follower.import_mined(h, body(&chain, n as u64 + 1), None).unwrap();
    }
    let mut forged = pos.header.clone();
    forged.difficulty = chain.difficulty_after(&head).unwrap();
    assert!(matches!(
        follower.import_mined(&forged, vec![], None),
        Err(ChainError::InvalidBlock(_))
    ));
    follower.import_block(&pos.header, vec![]).unwrap();
    // No mined reorganisation can undo a PoS block.
    assert!(matches!(
        follower.import_mined(&forged, vec![], None),
        Err(ChainError::InvalidBlock(_))
    ));
}

#[test]
fn gas_limit_moves_by_less_than_1_1024() {
    assert!(valid_gas_limit(30_000_000, 30_000_000));
    assert!(valid_gas_limit(30_000_000, 30_029_000));
    assert!(!valid_gas_limit(30_000_000, 30_029_297));
    assert!(!valid_gas_limit(15_000_000, 14_990_000), "below the floor");
    assert_eq!(next_gas_limit(30_000_000, 0), 30_000_000);
    assert_eq!(next_gas_limit(30_000_000, 60_000_000), 30_029_295);
    assert_eq!(next_gas_limit(30_000_000, 29_990_000), 29_990_000);
    assert_eq!(next_gas_limit(60_000_000, 90_000_000), 60_000_000);
    let mut g = 30_000_000;
    for _ in 0..1000 {
        let n = next_gas_limit(g, 45_000_000);
        assert!(valid_gas_limit(g, n));
        g = n;
    }
    assert_eq!(g, 45_000_000);

    // Blocks carry the producer's vote.
    let gs = genesis();
    let (_d, chain) = open(&gs);
    let (_f, follower) = open(&gs);
    chain.set_gas_target(60_000_000);
    let (h, _) = mine(&chain, vec![], Address::ZERO, 12);
    assert_eq!(h.gas_limit, 30_029_295);
    follower.import_mined(&h, vec![], None).unwrap();
    let mut greedy = h.clone();
    greedy.gas_limit = 31_000_000;
    let (_x, other) = open(&gs);
    assert!(matches!(other.import_mined(&greedy, vec![], None), Err(ChainError::InvalidBlock(_))));
}

/// Phase B genesis: T1 = 2 stakers / 128 BOLT, T2 = 3 stakers / 192 BOLT (pow-dev defaults).
fn genesis_b() -> Genesis {
    let mut g = genesis();
    g.config.pos_min_stakers = Some(3);
    g.config.pos_min_stake_bolt = Some(192);
    g.validate().unwrap();
    g
}

/// A QC by every member of `epoch`'s committee over `block` (validator ids 1, 2 hold dev keys
/// 20, 21).
fn checkpoint_qc(chain: &Chain, epoch: u64, round: u64, block: &Header) -> Vec<u8> {
    use bolt_consensus::{BlsScheme, Cert, Qc, Scheme, bitmap, vote_msg};
    let r = chain.store().reader().unwrap();
    let c = queries::committee_at(&StateView::latest(&r), 1338, epoch).unwrap().unwrap();
    let keys: Vec<_> = c.committee.members.iter().map(|id| dev_key(19 + id)).collect();
    let scheme = BlsScheme::new(c.pubkeys.clone(), &keys);
    let n = c.committee.members.len();
    let msg = vote_msg(1338, epoch, round, &block.hash_slow());
    let sigs: Vec<_> = (0..n as u16).map(|i| scheme.sign(i, &msg)).collect();
    let qc = Qc::<BlsScheme> {
        epoch,
        round,
        block: block.hash_slow(),
        height: block.number,
        signers: bitmap::encode(&(0..n as u16).collect::<Vec<_>>(), n),
        sig: scheme.aggregate(&sigs),
    };
    bolt_consensus::encode_cert(&Cert::Qc(qc))
}

/// Mines on the head with a certificate in the envelope.
fn mine_with_cert(chain: &Chain, cert: Vec<u8>, miner: Address) -> Result<(Header, MinedOutcome)> {
    let head = chain.head().unwrap();
    let t = chain.build_template(vec![], head.timestamp + 12, miner, Bytes::new(), cert)?;
    let key = chain.seal_key(&head.hash_slow(), head.number + 1).unwrap();
    let stop = AtomicBool::new(false);
    let (nonce, _) = chain
        .pow()
        .search(&key, &bolt_pow::seal_hash(&t.header), t.header.difficulty, 0, 1, u64::MAX, &stop)
        .unwrap()
        .unwrap();
    chain.seal_and_import(&t, nonce)
}

#[test]
fn phase_b_splits_rewards_and_pays_checkpoint_voters() {
    let g = genesis_b();
    let (_d, chain) = open(&g);
    let (_f, follower) = open(&g);
    let miner = Address::repeat_byte(0x33);
    mine(&chain, vec![register_tx(0, 20), register_tx(1, 21)], miner, 12);
    while chain.head().unwrap().number < 12 {
        mine(&chain, vec![], miner, 12);
    }
    // T1 held at blocks 5 and 9: phase B from epoch 4 (blocks 17..); T2 (3 stakers) never.
    let phase = chain.phase().unwrap();
    assert_eq!(phase.checkpoint_epoch, Some(4));
    assert_eq!(phase.pos_epoch, None);
    mine(&chain, vec![], miner, 12); // block 13 draws the epoch-4 committee
    // A certificate in a phase-A block (14, epoch 3) is invalid, even a well-signed one.
    let h13 = chain.head().unwrap();
    assert!(matches!(
        mine_with_cert(&chain, checkpoint_qc(&chain, 4, 1, &h13), miner),
        Err(ChainError::InvalidBlock(_))
    ));
    while chain.head().unwrap().number < 16 {
        mine(&chain, vec![], miner, 12);
    }
    let c4 = view(&chain, CONSENSUS, IConsensusRegistry::committeeCall { epoch: 4 });
    assert_eq!(c4.weights.iter().map(|w| *w as u32).sum::<u32>(), 4);

    // Epoch 4: the miner gets 60% of the block reward.
    let supply = view(&chain, REWARDS, IRewardDistributor::supplyCall {});
    let before = balance(&chain, miner);
    let (h17, _) = mine(&chain, vec![], miner, 12);
    let full = bolt_pow::block_reward(SUPPLY_CAP_WEI - supply, 8_000);
    assert_eq!(balance(&chain, miner) - before, full * U256::from(6) / U256::from(10));

    // Blocks carrying checkpoint QCs of epoch 4 record the votes, once per round.
    let qc1 = checkpoint_qc(&chain, 4, 1, &h17);
    mine_with_cert(&chain, qc1.clone(), miner).unwrap();
    mine_with_cert(&chain, qc1, miner).unwrap(); // same round again: not counted twice
    let h19 = chain.head().unwrap();
    mine_with_cert(&chain, checkpoint_qc(&chain, 4, 2, &h19), miner).unwrap();
    assert_eq!(
        view(&chain, REWARDS, IRewardDistributor::votesOfCall { epoch: 4, index: U256::ZERO }),
        U256::from(2)
    );
    // A forged QC (wrong epoch's committee keys) is invalid.
    let mut forged = checkpoint_qc(&chain, 4, 3, &h19);
    let last = forged.len() - 3;
    forged[last] ^= 1;
    assert!(mine_with_cert(&chain, forged, miner).is_err());

    // Block 25 settles epoch 4: the voters share 40% of the epoch's consensus emission.
    while chain.head().unwrap().number < 24 {
        mine(&chain, vec![], miner, 12);
    }
    let supply = view(&chain, REWARDS, IRewardDistributor::supplyCall {});
    mine(&chain, vec![], miner, 12);
    let emission = bolt_primitives::params::epoch_emission(SUPPLY_CAP_WEI - supply)
        * U256::from(8_000)
        / U256::from(10_000);
    let paid: U256 = [1u32, 2]
        .iter()
        .map(|id| view(&chain, REWARDS, IRewardDistributor::rewardsCall { id: *id }))
        .sum();
    // (the settling block's own reward and burn move the unissued pool slightly first)
    let expected = emission * U256::from(4) / U256::from(10);
    let diff = if paid > expected { paid - expected } else { expected - paid };
    assert!(diff < expected / U256::from(1_000), "paid {paid}, expected about {expected}");

    // A follower replays everything, certificates included.
    for n in 1..=chain.head().unwrap().number {
        let r = chain.store().reader().unwrap();
        let h = r.header(n).unwrap().unwrap();
        let root = r.envelope_root(n).unwrap().unwrap();
        let env = bolt_ipld::Envelope::decode(&r.ipld(&root).unwrap().unwrap()).unwrap();
        let txs = r.block(n).unwrap().unwrap().transactions;
        drop(r);
        follower.import_mined_with_cert(&h, txs, Some(root), env.qc).unwrap();
    }
    assert_eq!(follower.head().unwrap(), chain.head().unwrap());
}

#[test]
fn finalized_checkpoints_stop_reorganisations() {
    let g = genesis();
    let (_da, a) = open(&g);
    let (_db, b) = open(&g);
    for _ in 0..3 {
        let (h, _) = mine(&a, vec![], Address::repeat_byte(0xaa), 12);
        b.import_mined(&h, vec![], None).unwrap();
    }
    // A mines 3 more; B mines a heavier branch of 5 from block 3.
    for _ in 0..3 {
        mine(&a, vec![], Address::repeat_byte(0xaa), 12);
    }
    let mut branch = Vec::new();
    for _ in 0..5 {
        branch.push(mine(&b, vec![], Address::repeat_byte(0xbb), 10).0);
    }
    // Block 5 of A's chain is finalized: B's branch forks at 3, below it: refused.
    let a5 = a.store().reader().unwrap().header(5).unwrap().unwrap();
    assert!(a.finalize(&a5.hash_slow(), 5).unwrap());
    let first = a.import_mined(&branch[0], vec![], None);
    assert!(matches!(first, Err(ChainError::DeepReorg(_))), "{first:?}");
    assert_eq!(a.head().unwrap().number, 6);
    assert!(!a.finalize(&a5.hash_slow(), 5).unwrap(), "already final");

    // The other way round: B learns that A's block 4 is final. Even though B's own branch has
    // more work, B switches to A's chain (stake finality overrides work).
    let (_dc, c) = open(&g);
    for n in 1..=3 {
        let h = b.store().reader().unwrap().header(n).unwrap().unwrap();
        c.import_mined(&h, vec![], None).unwrap();
    }
    for h in &branch {
        c.import_mined(h, vec![], None).unwrap();
    }
    let a4 = a.store().reader().unwrap().header(4).unwrap().unwrap();
    c.import_mined(&a4, vec![], None).unwrap(); // side block
    assert_eq!(c.head().unwrap().number, 8);
    assert!(c.finalize(&a4.hash_slow(), 4).unwrap());
    assert_eq!(c.head().unwrap(), a4);
    assert_eq!(c.finalized().unwrap(), Some((4, a4.hash_slow())));
}

#[test]
fn one_rich_staker_cannot_start_a_committee() {
    // T1 needs 2 stakers: a single one with 50x the stake never gets a committee to capture.
    let g = genesis_b();
    let (_d, chain) = open(&g);
    let miner = Address::repeat_byte(0x33);
    mine(&chain, vec![register_tx_with(0, 20, bolt(5_000))], miner, 12);
    while chain.head().unwrap().number < 6 * L {
        mine(&chain, vec![], miner, 12);
    }
    let phase = chain.phase().unwrap();
    assert_eq!(phase.checkpoint_epoch, None);
    assert_eq!(phase.pos_epoch, None);
    assert_eq!(phase.checkpoint_streak, 0);
    for e in 0..8 {
        assert!(
            view(&chain, CONSENSUS, IConsensusRegistry::committeeCall { epoch: e }).ids.is_empty()
        );
    }
    // The miner kept the whole block reward (phase A).
    assert!(balance(&chain, miner) > U256::ZERO);
}
