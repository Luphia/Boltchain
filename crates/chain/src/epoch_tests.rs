//! Epochs on a single-producer chain (no consensus): committees, bootstrap exit, rewards and the
//! supply invariant (ADR 0006 §2, §8).

use super::*;
use alloy_consensus::{SignableTransaction, TxEip1559};
use alloy_network::TxSignerSync;
use alloy_primitives::TxKind;
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall;
use bolt_consensus::{BlsScheme, Cert, CommitProof, Qc, bitmap};
use bolt_primitives::bls::{dev_key, pubkey_point, signature_point};
use bolt_system::{abi::*, addresses::*, queries};

const L: u64 = 4;

fn genesis() -> Genesis {
    let mut g = Genesis::from_json(include_str!("../../../genesis/dev.json")).unwrap();
    g.config.epoch_slots = L;
    g.config.committee_size = 8;
    g.config.bootstrap_exit_min_stakers = Some(10);
    g.config.bootstrap_exit_min_stake_bolt = Some(640);
    g
}

fn bolt(n: u64) -> U256 {
    U256::from(n) * U256::from(10u64).pow(U256::from(18))
}

fn signer() -> PrivateKeySigner {
    "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".parse().unwrap()
}

fn call_tx(
    s: &PrivateKeySigner,
    nonce: u64,
    to: Address,
    value: U256,
    data: Vec<u8>,
) -> TxEnvelope {
    let mut t = TxEip1559 {
        chain_id: 1337,
        nonce,
        gas_limit: 3_000_000,
        max_fee_per_gas: 2_000_000_000,
        max_priority_fee_per_gas: 1_000_000,
        to: TxKind::Call(to),
        value,
        input: data.into(),
        ..Default::default()
    };
    let sig = s.sign_transaction_sync(&mut t).unwrap();
    t.into_signed(sig).into()
}

fn register_tx(s: &PrivateKeySigner, nonce: u64, key: u32) -> TxEnvelope {
    let sk = dev_key(key);
    let pk = sk.public_key();
    let data = IStakingManager::registerCall {
        pubkey: Bytes::copy_from_slice(pk.as_slice()),
        pubkeyPoint: Bytes::copy_from_slice(&pubkey_point(&pk).unwrap()),
        pop: Bytes::copy_from_slice(&signature_point(&sk.proof_of_possession()).unwrap()),
        feeRecipient: Address::repeat_byte(0x50 + key as u8),
    }
    .abi_encode();
    call_tx(s, nonce, STAKING, bolt(100), data)
}

/// A certificate for `parent` in which every member of its epoch's committee voted.
fn cert_for(chain: &Chain, parent: &Header) -> Vec<u8> {
    if parent.number == 0 {
        return Vec::new();
    }
    let epoch = chain.rules().epoch_of(parent.number);
    let r = chain.store().reader().unwrap();
    let c = queries::committee_at(&StateView::latest(&r), 1337, epoch).unwrap().unwrap();
    let n = c.committee.members.len();
    let signers: Vec<u16> = (0..n as u16).collect();
    let qc = Qc::<BlsScheme> {
        epoch,
        round: parent.number,
        block: parent.hash_slow(),
        height: parent.number,
        signers: bitmap::encode(&signers, n),
        sig: Some(Default::default()),
    };
    // The first block of an epoch carries an epoch proof instead (its votes count the same).
    let cert = if chain.rules().is_epoch_start(parent.number + 1) {
        Cert::Epoch(CommitProof {
            qc: qc.clone(),
            child_qc: qc,
            child_header: vec![],
            nil_rounds: vec![parent.number + 1],
            committed: parent.hash_slow(),
            committed_height: parent.number,
        })
    } else {
        Cert::Qc(qc)
    };
    bolt_consensus::encode_cert(&cert)
}

fn produce(chain: &Chain, txs: Vec<TxEnvelope>) -> Header {
    let head = chain.head().unwrap();
    let qc = cert_for(chain, &head);
    let candidates: Vec<_> = txs
        .into_iter()
        .map(|t| {
            let s = t.recover_signer().unwrap();
            (t, s)
        })
        .collect();
    let n = candidates.len();
    let built = chain
        .build_on(
            &head.hash_slow(),
            candidates,
            head.timestamp + 6,
            Address::repeat_byte(0xbe),
            Bytes::new(),
            qc,
        )
        .unwrap();
    assert_eq!(built.included.len(), n, "rejected: {:?}", built.rejected);
    chain.commit_pending(&built.hash).unwrap()
}

fn view<C: SolCall>(chain: &Chain, to: Address, c: C) -> C::Return {
    let r = chain.store().reader().unwrap();
    queries::call(&StateView::latest(&r), 1337, to, c).unwrap()
}

fn balance(chain: &Chain, a: Address) -> U256 {
    let r = chain.store().reader().unwrap();
    revm::DatabaseRef::basic_ref(&StateView::latest(&r), a)
        .unwrap()
        .map(|i| i.balance)
        .unwrap_or_default()
}

#[test]
fn epochs_rotate_committees_and_pay_rewards() {
    let g = genesis();
    let dir = tempfile::tempdir().unwrap();
    let chain = Chain::open(dir.path(), &g).unwrap();
    let s = signer();
    // Every address that can hold BOLT in this test.
    let mut tracked: Vec<Address> = g.alloc.keys().copied().collect();
    tracked.extend([STAKING, REWARDS, CONSENSUS, Address::repeat_byte(0xbe)]);
    tracked.extend(g.bootstrap_validators.iter().map(|v| v.fee_recipient));
    tracked.extend((20..30).map(|k| Address::repeat_byte(0x50 + k as u8)));
    let invariant = |chain: &Chain| {
        let total: U256 = tracked.iter().map(|a| balance(chain, *a)).sum();
        let supply = view(chain, REWARDS, IRewardDistributor::supplyCall {});
        let dead = view(chain, STAKING, IStakingManager::deadStakeCall {});
        // The head's own burned base fee leaves the supply in the next block's system call.
        let head = chain.head().unwrap();
        let pending_burn =
            U256::from(head.base_fee_per_gas.unwrap_or(0)) * U256::from(head.gas_used);
        assert_eq!(total + pending_burn, supply + dead, "balances vs supply at {}", head.number);
    };
    invariant(&chain);

    // Epoch 0 (blocks 1-4): ten validators register in block 1.
    let txs: Vec<_> = (0..10).map(|i| register_tx(&s, i, 20 + i as u32)).collect();
    produce(&chain, txs);
    invariant(&chain);
    assert_eq!(view(&chain, STAKING, IStakingManager::countCall {}), 17);
    // Block 1 started epoch 0 and wrote committee 1: still the bootstrap set (no stakers yet).
    let c1 = view(&chain, CONSENSUS, IConsensusRegistry::committeeCall { epoch: 1 });
    assert_eq!(c1.ids, (1..=7).collect::<Vec<u32>>());
    for _ in 2..=4 {
        produce(&chain, vec![]);
        invariant(&chain);
    }
    assert!(!view(&chain, CONSENSUS, IConsensusRegistry::bootstrapEndedCall {}));

    // Block 5 starts epoch 1: epoch 0 is settled (bootstrap: 25%, locked into stake) and, with 10
    // stakers and 1,000 BOLT staked, the bootstrap phase ends: committee 2 is sampled by stake.
    let supply_before = view(&chain, REWARDS, IRewardDistributor::supplyCall {});
    produce(&chain, vec![]);
    invariant(&chain);
    assert_eq!(view(&chain, CONSENSUS, IConsensusRegistry::currentEpochCall {}), 1);
    assert!(view(&chain, CONSENSUS, IConsensusRegistry::bootstrapEndedCall {}));
    // Sampled by weight from every eligible validator: the ten new ones (100 BOLT each) and the
    // bootstrap validators, whose locked rewards count only up to the cap (ADR 0006 §11).
    let c2 = view(&chain, CONSENSUS, IConsensusRegistry::committeeCall { epoch: 2 });
    assert!(c2.ids.iter().all(|id| (1..=17).contains(id)), "{:?}", c2.ids);
    assert_eq!(
        c2.weights.iter().map(|w| *w as u32).sum::<u32>(),
        8,
        "8 seats, not the 7 equal bootstrap seats"
    );
    assert_eq!(c2.seats.len(), 16);
    let emission = bolt_primitives::params::epoch_emission(
        bolt_primitives::params::SUPPLY_CAP_WEI - supply_before,
    ) * U256::from(8_000)
        / U256::from(10_000);
    let paid = view(&chain, REWARDS, IRewardDistributor::supplyCall {}) - supply_before;
    // four blocks' votes each (blocks 2-5 certify heights 1-4); 25% for bootstrap validators
    let expected = emission / U256::from(7) * U256::from(7) / U256::from(4);
    assert!(
        paid > expected - U256::from(100) && paid <= emission / U256::from(4),
        "paid {paid}, expected ~{expected}"
    );
    let v1 = view(&chain, STAKING, IStakingManager::validatorCall { id: 1 });
    assert_eq!(v1.locked, v1.stake);
    assert!(v1.locked > bolt(10_000), "one epoch of bootstrap rewards is large");
    // ...but it weighs no more than an average staker at the exit threshold (640 / 10 BOLT).
    let cap = view(&chain, PARAMS, IParamRegistry::lockedWeightCapCall {});
    assert_eq!(U256::from(cap), bolt(64));
    assert_eq!(view(&chain, STAKING, IStakingManager::weightOfCall { id: 1 }), bolt(64));
    assert_eq!(view(&chain, STAKING, IStakingManager::weightOfCall { id: 8 }), bolt(100));
    let snap = view(&chain, STAKING, IStakingManager::snapshotCall {});
    let w1 = snap.ids.iter().position(|i| *i == 1).map(|k| snap.stakes[k]);
    assert_eq!(w1, Some(bolt(64)), "the lottery sees the capped weight");
    assert_eq!(view(&chain, REWARDS, IRewardDistributor::rewardsCall { id: 1 }), U256::ZERO);

    // Blocks 6-9: epoch 1 is still the bootstrap committee, settled at block 9 at full rate now
    // that the phase is over (claimable, not locked).
    for _ in 6..=9 {
        produce(&chain, vec![]);
        invariant(&chain);
    }
    let r1 = view(&chain, REWARDS, IRewardDistributor::rewardsCall { id: 1 });
    assert!(r1 > U256::ZERO);
    // Claim sends it to the fee recipient.
    let fr = g.bootstrap_validators[0].fee_recipient;
    let before = balance(&chain, fr);
    produce(
        &chain,
        vec![call_tx(
            &s,
            10,
            REWARDS,
            U256::ZERO,
            IRewardDistributor::claimCall { id: 1 }.abi_encode(),
        )],
    );
    invariant(&chain);
    assert_eq!(balance(&chain, fr) - before, r1);

    // Epoch 2 (blocks 9-12) runs with the sampled committee; block 13 pays its members by seats.
    for _ in 11..=12 {
        produce(&chain, vec![]);
        invariant(&chain);
    }
    let c2 = view(&chain, CONSENSUS, IConsensusRegistry::committeeCall { epoch: 2 });
    let rewards_of = |chain: &Chain| -> Vec<U256> {
        (8..=17u32)
            .chain(1..=7)
            .map(|id| view(chain, REWARDS, IRewardDistributor::rewardsCall { id }))
            .collect()
    };
    let before = rewards_of(&chain);
    produce(&chain, vec![]);
    invariant(&chain);
    let after = rewards_of(&chain);
    let order: Vec<u32> = (8..=17u32).chain(1..=7).collect();
    let delta = |id: u32| {
        let i = order.iter().position(|x| *x == id).unwrap();
        after[i] - before[i]
    };
    let rewards: Vec<U256> = c2.ids.iter().map(|id| delta(*id)).collect();
    for (i, r) in rewards.iter().enumerate() {
        assert!(*r > U256::ZERO, "member {} got nothing", c2.ids[i]);
    }
    // proportional to seats (all voted equally often)
    let (a, b) = (0, c2.ids.len() - 1);
    let lhs = rewards[a] * U256::from(c2.weights[b]);
    let rhs = rewards[b] * U256::from(c2.weights[a]);
    let diff = if lhs > rhs { lhs - rhs } else { rhs - lhs };
    assert!(diff <= U256::from(16), "rewards not proportional to seats");
    // non-members got nothing for epoch 2
    for id in order.iter().copied().filter(|id| !c2.ids.contains(id)) {
        assert_eq!(delta(id), U256::ZERO, "validator {id}");
    }
}
