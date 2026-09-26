//! History (ADR 0009): epoch indexes recorded on chain, audit tasks, panel certificates, storage
//! rewards and penalties, on a dev chain with genesis validators (keys `dev_key(0..7)`).

use super::epoch_tests::{balance, bolt, cert_for, genesis, produce, view};
use super::*;
use bolt_consensus::{AuditCert, AuditVote};
use bolt_primitives::bls::dev_key;
use bolt_system::{abi::*, addresses::*};

const L: u64 = 4;

fn chain_with_history() -> (tempfile::TempDir, Chain, Genesis) {
    let mut g = genesis();
    g.config.history_recent_epochs = Some(1);
    let d = tempfile::tempdir().unwrap();
    let c = Chain::open(d.path(), &g).unwrap();
    (d, c, g)
}

/// Votes of the whole panel of `epoch` on task `task`.
fn certify(chain: &Chain, epoch: u64, task: u16, passed: bool) -> Vec<u8> {
    let a = view(chain, SWARM, ISwarmStorage::auditsCall { epoch });
    let votes: Vec<AuditVote> = a
        .panel
        .iter()
        .enumerate()
        .map(|(m, id)| AuditVote::sign(&dev_key(id - 1), 1337, epoch, task, passed, m as u16))
        .collect();
    AuditCert::aggregate(&votes, a.panel.len()).unwrap().encode()
}

#[test]
fn epoch_indexes_audits_and_storage_rewards() {
    let (_d, chain, _g) = chain_with_history();
    // Epoch 0 = blocks 1..4; block 5 records its index.
    while chain.head().unwrap().number < L + 1 {
        produce(&chain, vec![]);
    }
    assert_eq!(view(&chain, SWARM, ISwarmStorage::indexedEpochsCall {}), 1);
    let cid = view(&chain, SWARM, ISwarmStorage::epochIndexCall { epoch: 0 });
    let cid = bolt_ipld::Cid::try_from(cid.as_ref()).unwrap();
    let r = chain.store().reader().unwrap();
    let idx = bolt_ipld::history::EpochIndex::decode(&r.ipld(&cid).unwrap().unwrap()).unwrap();
    assert_eq!((idx.epoch, idx.first, idx.count), (0, 1, L));
    let group =
        bolt_ipld::history::decode_group(&r.ipld(&idx.groups[0]).unwrap().unwrap()).unwrap();
    let expected: Vec<_> = (1..=L).map(|n| r.envelope_root(n).unwrap().unwrap()).collect();
    assert_eq!(group, expected, "the index lists the epoch's envelopes in order");
    drop(r);

    // Block 9 starts epoch 2: epoch 0 is older than the 1-epoch window, so audits are drawn.
    while chain.head().unwrap().number < 2 * L + 1 {
        produce(&chain, vec![]);
    }
    let a = view(&chain, SWARM, ISwarmStorage::auditsCall { epoch: 2 });
    assert_eq!(a.providers.len(), 16);
    assert!(a.targets.iter().all(|t| *t == 0));
    assert!(a.heights.iter().all(|h| (1..=L).contains(h)));
    let committee = view(&chain, CONSENSUS, IConsensusRegistry::committeeCall { epoch: 2 });
    assert!(a.panel.iter().all(|id| committee.ids.contains(id)));
    assert!(a.states.iter().all(|s| *s == 0));

    // Pass on task 0, fail on the first task whose provider differs.
    let pass_provider = a.providers[0];
    let fail_task = a.providers.iter().position(|p| *p != pass_provider).unwrap_or(1) as u16;
    let fail_provider = a.providers[fail_task as usize];
    let stake_before =
        view(&chain, STAKING, IStakingManager::validatorCall { id: fail_provider }).stake;
    chain.add_audit_cert(certify(&chain, 2, 0, true));
    chain.add_audit_cert(certify(&chain, 2, fail_task, false));
    chain.add_audit_cert(vec![1, 2, 3]); // garbage: a producer just drops it
    let h = produce(&chain, vec![]);
    let a = view(&chain, SWARM, ISwarmStorage::auditsCall { epoch: 2 });
    assert_eq!(a.states[0], 1, "passed");
    assert_eq!(a.states[fail_task as usize], 2, "failed");
    let v = view(&chain, STAKING, IStakingManager::validatorCall { id: fail_provider });
    assert_eq!(v.stake, stake_before - stake_before / U256::from(100), "1% penalty");
    assert!(matches!(v.status, IStakingManager::Status::Active), "no forced exit");
    // The same certificates again are ignored (already decided).
    let r = chain.store().reader().unwrap();
    let env = bolt_ipld::Envelope::decode(
        &r.ipld(&r.envelope_root(h.number).unwrap().unwrap()).unwrap().unwrap(),
    )
    .unwrap();
    assert_eq!(env.audits.len(), 2, "the two valid certificates are in the block");
    drop(r);
    let h2 = produce(&chain, vec![]);
    let r = chain.store().reader().unwrap();
    let env2 = bolt_ipld::Envelope::decode(
        &r.ipld(&r.envelope_root(h2.number).unwrap().unwrap()).unwrap().unwrap(),
    )
    .unwrap();
    assert!(env2.audits.is_empty());
    drop(r);

    // Block 13 settles epoch 2: the storage reward (1 BOLT per block of the epoch) goes to the pass.
    let before = view(&chain, REWARDS, IRewardDistributor::rewardsCall { id: pass_provider });
    let supply = view(&chain, REWARDS, IRewardDistributor::supplyCall {});
    while chain.head().unwrap().number < 3 * L + 1 {
        produce(&chain, vec![]);
    }
    let after = view(&chain, REWARDS, IRewardDistributor::rewardsCall { id: pass_provider });
    let _ = supply;
    let storage = bolt_primitives::params::storage_reward() * U256::from(chain.rules().epoch_slots);
    let gained = after - before;
    assert!(
        gained > storage * U256::from(99) / U256::from(100),
        "storage reward {gained} of {storage}"
    );
    let _ = balance;
    let _ = bolt;
}

#[test]
fn received_blocks_must_carry_only_valid_audit_certificates() {
    let (_d, a, g) = chain_with_history();
    let (_e, b) = {
        let d = tempfile::tempdir().unwrap();
        let c = Chain::open(d.path(), &g).unwrap();
        (d, c)
    };
    while a.head().unwrap().number < 2 * L + 1 {
        let h = produce(&a, vec![]);
        let r = a.store().reader().unwrap();
        let env = bolt_ipld::Envelope::decode(
            &r.ipld(&r.envelope_root(h.number).unwrap().unwrap()).unwrap().unwrap(),
        )
        .unwrap();
        drop(r);
        b.import_final(&h, vec![], None, Certs::of(&env)).unwrap();
    }
    // Producer A includes a valid certificate; B imports it with the envelope's certificates...
    a.add_audit_cert(certify(&a, 2, 3, true));
    let h = produce(&a, vec![]);
    let r = a.store().reader().unwrap();
    let root = r.envelope_root(h.number).unwrap().unwrap();
    let env = bolt_ipld::Envelope::decode(&r.ipld(&root).unwrap().unwrap()).unwrap();
    drop(r);
    // ...but not with a forged one in its place.
    let mut forged = Certs::of(&env);
    let mut c = AuditCert::decode(&forged.audits[0]).unwrap();
    c.passed = false;
    forged.audits[0] = c.encode();
    assert!(matches!(
        b.import_final(&h, vec![], Some(root), forged),
        Err(ChainError::InvalidBlock(_))
    ));
    b.import_final(&h, vec![], Some(root), Certs::of(&env)).unwrap();
    let _ = cert_for;
}

/// Copies IPFS blocks from one node's blockstore to another's (what Bitswap does).
fn copy_blocks(from: &Chain, to: &Chain, cids: &[bolt_ipld::Cid]) {
    let r = from.store().reader().unwrap();
    let w = to.store().writer().unwrap();
    for c in cids {
        w.put_ipld(c, &r.ipld(c).unwrap().unwrap()).unwrap();
    }
    w.commit().unwrap();
}

fn block_cids(chain: &Chain, n: u64) -> Vec<bolt_ipld::Cid> {
    let r = chain.store().reader().unwrap();
    let root = r.envelope_root(n).unwrap().unwrap();
    let env = bolt_ipld::Envelope::decode(&r.ipld(&root).unwrap().unwrap()).unwrap();
    [root, env.header].into_iter().chain(env.chunks).collect()
}

#[test]
fn a_new_node_starts_from_a_snapshot_and_follows() {
    use super::epoch_tests::{call_tx, signer};
    let (_d, a, g) = chain_with_history();
    let s = signer();
    let mut nonce = 0;
    let mut transfer = |to: u8| {
        nonce += 1;
        vec![call_tx(&s, nonce - 1, Address::repeat_byte(to), bolt(1), vec![])]
    };
    // Epochs 0 and 1 (blocks 1..=8), with transfers so bodies have chunks.
    while a.head().unwrap().number < 2 * L {
        let n = a.head().unwrap().number as u8;
        produce(&a, transfer(n + 1));
    }
    assert_eq!(a.take_snapshot().unwrap().map(|(n, _)| n), Some(2 * L));
    assert!(a.take_snapshot().unwrap().is_none(), "once per block");
    let (number, root) = *a.snapshots().unwrap().last().unwrap();
    let snap = bolt_ipld::history::SnapshotRoot::decode(
        &a.store().reader().unwrap().ipld(&root).unwrap().unwrap(),
    )
    .unwrap();
    let trusted = a.store().reader().unwrap().block_hash(number).unwrap().unwrap();

    // A fresh node receives the snapshot, the checkpoint block and its ancestors' envelopes.
    let d = tempfile::tempdir().unwrap();
    let b = Chain::open(d.path(), &g).unwrap();
    let mut cids = vec![root];
    cids.extend(Chain::snapshot_parts(&snap));
    for n in b.backfill_from(number)..=number {
        cids.extend(block_cids(&a, n));
    }
    copy_blocks(&a, &b, &cids);
    // A wrong trusted hash is refused.
    assert!(b.import_checkpoint(&root, &B256::repeat_byte(1)).is_err());
    let h = b.import_checkpoint(&root, &trusted).unwrap();
    assert_eq!(h.hash_slow(), trusted);
    assert_eq!(b.head().unwrap().state_root, a.head().unwrap().state_root);
    assert_eq!(b.finalized().unwrap(), Some((number, trusted)));
    assert_eq!(b.store().reader().unwrap().base().unwrap(), number);

    // It follows the chain from there: blocks 9.. (block 9 records epoch 1's index).
    while a.head().unwrap().number < 3 * L + 2 {
        let n = a.head().unwrap().number as u8;
        let h = produce(&a, transfer(n + 1));
        let r = a.store().reader().unwrap();
        let root = r.envelope_root(h.number).unwrap().unwrap();
        let env = bolt_ipld::Envelope::decode(&r.ipld(&root).unwrap().unwrap()).unwrap();
        let txs = r.block(h.number).unwrap().unwrap().transactions;
        drop(r);
        b.import_final(&h, txs, None, Certs::of(&env)).unwrap();
        assert_eq!(b.head().unwrap().hash_slow(), h.hash_slow());
    }

    // Pruning epoch 0 on A: bodies and chunks go, headers and envelopes stay.
    assert!(a.missing_epoch_data(0).unwrap().is_empty());
    let saved: Vec<(bolt_ipld::Cid, Vec<u8>)> = {
        let r = a.store().reader().unwrap();
        (1..=L).flat_map(|n| block_cids(&a, n)).map(|c| (c, r.ipld(&c).unwrap().unwrap())).collect()
    };
    assert_eq!(a.prune_epoch(0).unwrap(), L);
    let missing = a.missing_epoch_data(0).unwrap();
    assert_eq!(missing.len() as u64, L, "one chunk per block");
    assert!(a.store().reader().unwrap().header(1).unwrap().is_some());
    // Restoring the chunks (from a peer) completes the epoch again.
    let w = a.store().writer().unwrap();
    for (c, d) in &saved {
        w.put_ipld(c, d).unwrap();
    }
    w.commit().unwrap();
    assert!(a.missing_epoch_data(0).unwrap().is_empty());

    // Snapshots are kept up to KEEP_SNAPSHOTS.
    while a.head().unwrap().number < 4 * L {
        produce(&a, vec![]);
    }
    a.take_snapshot().unwrap().unwrap();
    while a.head().unwrap().number < 5 * L {
        produce(&a, vec![]);
    }
    a.take_snapshot().unwrap().unwrap();
    let kept = a.snapshots().unwrap();
    assert_eq!(kept.iter().map(|(n, _)| *n).collect::<Vec<_>>(), vec![4 * L, 5 * L]);
    assert!(a.store().reader().unwrap().ipld(&root).unwrap().is_none(), "oldest deleted");
}
