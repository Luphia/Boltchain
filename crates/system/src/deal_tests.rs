//! SwarmStorage deals (ADR 0014): offers, assignment, per-epoch claims, bond building, failed
//! audits (penalty, forfeited epochs, replacement), repair, cancel, extend and close.

use crate::{abi::*, addresses::*, tests::Evm};
use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::SolCall;

const BASE: u32 = bolt_primitives::params::STORAGE_ID_BASE;
const GIB_PRICE: u128 = 1_000_000_000_000_000_000; // 1 BOLT per GiB per epoch

fn bolt(n: u64) -> U256 {
    U256::from(n) * U256::from(10u128.pow(18))
}

struct Net {
    evm: Evm,
    /// Storage providers without stake: (id, account).
    providers: Vec<(u32, Address)>,
    owner: Address,
}

fn set_epoch(evm: &mut Evm, epoch: u64) {
    evm.system(
        CONSENSUS,
        IConsensusRegistry::beginEpochCall {
            epoch,
            checkpointMet: false,
            posMet: false,
            streakRequired: 1000,
        },
    );
}

/// `n` storage providers without stake, each with a 1 BOLT bond and an open offer of 1 GiB at
/// 0.5 BOLT per GiB per epoch.
fn net(n: u8) -> Net {
    let mut evm = Evm::from_genesis(&crate::tests::dev_genesis());
    evm.timestamp = evm.timestamp.max(1_000_000);
    set_epoch(&mut evm, 10);
    let mut providers = Vec::new();
    for i in 0..n {
        let account = Address::repeat_byte(0x40 + i);
        evm.fund(account, bolt(2));
        let peer = Bytes::from(vec![i; 4]);
        let id = evm.call(
            account,
            SWARM,
            U256::ZERO,
            ISwarmStorage::registerStorageCall { peerId: peer },
        );
        evm.call(account, SWARM, bolt(1), ISwarmStorage::depositBondCall { id });
        evm.call(
            account,
            SWARM,
            U256::ZERO,
            ISwarmStorage::offerStorageCall { id, capacityMiB: 1024, minPrice: GIB_PRICE / 2 },
        );
        providers.push((id, account));
    }
    let owner = Address::repeat_byte(0xd0);
    evm.fund(owner, bolt(1_000));
    Net { evm, providers, owner }
}

impl Net {
    /// A 3 MiB deal of 10 blocks: `replicas` copies for `epochs` epochs at 1 BOLT per GiB.
    fn create(&mut self, replicas: u8, epochs: u64) -> Option<U256> {
        let c = ISwarmStorage::createDealCall {
            root: Bytes::from_static(b"deal-root-cid"),
            blocks: 10,
            size: 3 * 1024 * 1024,
            replicas,
            epochs,
            price: GIB_PRICE,
        };
        let cost = per_epoch() * U256::from(replicas) * U256::from(epochs);
        let (ok, out) = self.evm.tx(self.owner, SWARM, cost, c.abi_encode());
        ok.then(|| ISwarmStorage::createDealCall::abi_decode_returns(&out).unwrap())
    }

    fn slots(&mut self, id: U256) -> ISwarmStorage::dealSlotsReturn {
        self.evm.view(SWARM, ISwarmStorage::dealSlotsCall { id })
    }

    fn account(&self, provider: u32) -> Address {
        self.providers.iter().find(|(id, _)| *id == provider).unwrap().1
    }

    fn in_contract(&mut self, a: Address) -> U256 {
        self.evm.view(SWARM, ISwarmStorage::balanceOfCall { account: a })
    }
}

/// 3 MiB at 1 BOLT per GiB: 3/1024 BOLT per copy per epoch (rounded up).
fn per_epoch() -> U256 {
    (U256::from(GIB_PRICE) * U256::from(3) + U256::from(1023)) / U256::from(1024)
}

#[test]
fn a_deal_is_assigned_paid_per_epoch_and_closed() {
    let mut n = net(4);
    let id = n.create(2, 10).expect("enough offers");
    let d = n.evm.view(SWARM, ISwarmStorage::dealCall { id });
    assert_eq!((d.startEpoch, d.endEpoch, d.replicas), (10, 20, 2));
    assert_eq!(U256::from(d.perEpoch), per_epoch());
    assert_eq!(d.escrow, per_epoch() * U256::from(20));
    let s = n.slots(id);
    assert_eq!(s.providers.len(), 2);
    assert_ne!(s.providers[0], s.providers[1], "distinct providers");
    assert!(s.providers.iter().all(|p| *p >= BASE));
    let o = n.evm.view(SWARM, ISwarmStorage::offerCall { id: s.providers[0] });
    assert_eq!(o.usedMiB, 3);
    assert_eq!(o.committed, per_epoch() * U256::from(10));
    assert_eq!(n.evm.view(SWARM, ISwarmStorage::dealsOfCall { id: s.providers[0] }), vec![id]);

    // Three whole epochs later anyone claims copy 0: 20% of it builds the bond.
    set_epoch(&mut n.evm, 13);
    let p0 = s.providers[0];
    let bond0 = n.evm.view(SWARM, ISwarmStorage::storageProviderCall { id: p0 }).bond;
    n.evm.fund(Address::repeat_byte(0xee), bolt(1));
    n.evm.call(
        Address::repeat_byte(0xee),
        SWARM,
        U256::ZERO,
        ISwarmStorage::claimCall { id, slot: U256::ZERO },
    );
    let earned = per_epoch() * U256::from(3);
    let to_bond = earned * U256::from(2_000) / U256::from(10_000);
    let bond1 = n.evm.view(SWARM, ISwarmStorage::storageProviderCall { id: p0 }).bond;
    assert_eq!(bond1 - bond0, to_bond);
    let acc0 = n.account(p0);
    assert_eq!(n.in_contract(acc0), earned - to_bond);
    // Claiming again in the same epoch pays nothing more.
    n.evm.call(acc0, SWARM, U256::ZERO, ISwarmStorage::claimCall { id, slot: U256::ZERO });
    assert_eq!(n.in_contract(acc0), earned - to_bond);

    // Not closable before the end; after it, both copies are paid in full and capacity freed.
    assert!(!n.evm.try_call(n.owner, SWARM, U256::ZERO, ISwarmStorage::closeDealCall { id }));
    set_epoch(&mut n.evm, 25);
    n.evm.call(n.owner, SWARM, U256::ZERO, ISwarmStorage::closeDealCall { id });
    let acc1 = n.account(s.providers[1]);
    let full = per_epoch() * U256::from(10);
    assert_eq!(n.in_contract(acc1), full - full * U256::from(2_000) / U256::from(10_000));
    assert_eq!(n.in_contract(n.owner), U256::ZERO, "nothing left to refund");
    let o = n.evm.view(SWARM, ISwarmStorage::offerCall { id: p0 });
    assert_eq!((o.usedMiB, o.committed), (0, U256::ZERO));
    assert!(n.evm.view(SWARM, ISwarmStorage::dealCall { id }).closed);

    // Payments leave by withdraw (or payout by anyone).
    let before = n.evm.balance(acc1);
    n.evm.call(
        Address::repeat_byte(0xee),
        SWARM,
        U256::ZERO,
        ISwarmStorage::payoutCall { account: acc1 },
    );
    assert_eq!(n.evm.balance(acc1) - before, full - full * U256::from(2_000) / U256::from(10_000));
}

#[test]
fn assignment_needs_enough_eligible_offers() {
    let mut n = net(2);
    assert!(n.create(3, 10).is_none(), "three copies need three providers");
    // Price below every offer's minimum.
    let c = ISwarmStorage::createDealCall {
        root: Bytes::from_static(b"r"),
        blocks: 1,
        size: 1,
        replicas: 1,
        epochs: 1,
        price: GIB_PRICE / 4,
    };
    assert!(!n.evm.try_call(n.owner, SWARM, bolt(1), c));
    // Underpaid escrow.
    let c = ISwarmStorage::createDealCall {
        root: Bytes::from_static(b"r"),
        blocks: 1,
        size: 3 * 1024 * 1024,
        replicas: 1,
        epochs: 10,
        price: GIB_PRICE,
    };
    assert!(!n.evm.try_call(n.owner, SWARM, per_epoch() * U256::from(9), c));
    // A closed offer takes no new deals.
    let (id0, acc0) = n.providers[0];
    n.evm.call(acc0, SWARM, U256::ZERO, ISwarmStorage::closeOfferCall { id: id0 });
    assert!(n.create(2, 10).is_none());
    assert!(n.create(1, 10).is_some());
    // Only the provider's account manages its offer.
    let c = ISwarmStorage::offerStorageCall { id: id0, capacityMiB: 1, minPrice: 1 };
    assert!(!n.evm.try_call(n.owner, SWARM, U256::ZERO, c));
}

#[test]
fn unpaid_value_is_limited_by_the_bond() {
    // 1 BOLT bond: at most 5 + 10 = 15 BOLT of unpaid deal value per provider.
    let mut n = net(1);
    let big = |epochs: u64| ISwarmStorage::createDealCall {
        root: Bytes::from_static(b"r"),
        blocks: 1,
        size: 1024 * 1024 * 1024, // 1 GiB: 1 BOLT per epoch
        replicas: 1,
        epochs,
        price: GIB_PRICE,
    };
    assert!(!n.evm.try_call(n.owner, SWARM, bolt(16), big(16)));
    assert!(n.evm.try_call(n.owner, SWARM, bolt(15), big(15)));
}

#[test]
fn a_failed_deal_audit_penalises_forfeits_and_replaces() {
    let mut n = net(3);
    let id = n.create(2, 10).unwrap();
    let s = n.slots(id);
    let bad = s.providers[1];
    set_epoch(&mut n.evm, 14);
    // Epoch 14: no history tasks, one deal task on `bad`.
    n.evm.system(
        SWARM,
        ISwarmStorage::beginAuditsCall {
            epoch: 14,
            panel: vec![1],
            providers: vec![],
            targets: vec![],
            heights: vec![],
        },
    );
    n.evm.system(
        SWARM,
        ISwarmStorage::addDealAuditsCall {
            epoch: 14,
            providers: vec![bad],
            dealIds: vec![id.to::<u64>()],
            indexes_: vec![7],
        },
    );
    assert_eq!(n.evm.view(SWARM, ISwarmStorage::taskKindsCall { epoch: 14 }), vec![1u8]);
    let a = n.evm.view(SWARM, ISwarmStorage::auditsCall { epoch: 14 });
    assert_eq!((a.providers[0], a.targets[0], a.heights[0]), (bad, id.to::<u64>(), 7));
    let supply = n.evm.view(REWARDS, IRewardDistributor::supplyCall {});
    assert!(n.evm.system(SWARM, ISwarmStorage::recordAuditCall { epoch: 14, task: 0, ok: false }));

    // Penalty: 10% of the 1 BOLT bond burned, deactivated.
    let p = n.evm.view(SWARM, ISwarmStorage::storageProviderCall { id: bad });
    assert_eq!(p.bond, bolt(1) - bolt(1) / U256::from(10));
    assert!(!p.active);
    assert_eq!(
        n.evm.view(REWARDS, IRewardDistributor::supplyCall {}),
        supply - bolt(1) / U256::from(10)
    );
    // Its four unclaimed epochs are forfeited; the third provider took over from epoch 14.
    let s2 = n.slots(id);
    assert!(s2.open[1]);
    assert_ne!(s2.providers[1], bad);
    assert_eq!(s2.since[1], 14);
    assert_eq!(n.in_contract(n.account(bad)), U256::ZERO);
    let o = n.evm.view(SWARM, ISwarmStorage::offerCall { id: bad });
    assert_eq!((o.usedMiB, o.committed), (0, U256::ZERO));
    // A deal pass earns no emission (only history passes count).
    assert!(n.evm.view(SWARM, ISwarmStorage::passedCall { epoch: 14 }).is_empty());

    // At the end the owner gets the forfeited epochs back.
    set_epoch(&mut n.evm, 20);
    n.evm.call(n.owner, SWARM, U256::ZERO, ISwarmStorage::closeDealCall { id });
    assert_eq!(n.in_contract(n.owner), per_epoch() * U256::from(4));
}

#[test]
fn repair_cancel_and_extend() {
    let mut n = net(3);
    let id = n.create(1, 10).unwrap();
    let p = n.slots(id).providers[0];
    // Repair only replaces providers that left.
    assert!(!n.evm.try_call(
        n.owner,
        SWARM,
        U256::ZERO,
        ISwarmStorage::repairCall { id, slot: U256::ZERO }
    ));
    set_epoch(&mut n.evm, 12);
    let acc = n.account(p);
    n.evm.call(acc, SWARM, U256::ZERO, ISwarmStorage::exitStorageCall { id: p });
    // Gone providers cannot claim.
    assert!(!n.evm.try_call(
        acc,
        SWARM,
        U256::ZERO,
        ISwarmStorage::claimCall { id, slot: U256::ZERO }
    ));
    n.evm.call(n.owner, SWARM, U256::ZERO, ISwarmStorage::repairCall { id, slot: U256::ZERO });
    let s = n.slots(id);
    assert_ne!(s.providers[0], p);
    assert_eq!(s.since[0], 12);

    // Extend by 5 epochs, then cancel: the deal ends after the current epoch.
    let c = ISwarmStorage::extendDealCall { id, epochs: 5 };
    assert!(!n.evm.try_call(n.owner, SWARM, per_epoch() * U256::from(4), c.clone()));
    n.evm.call(n.owner, SWARM, per_epoch() * U256::from(5), c);
    assert_eq!(n.evm.view(SWARM, ISwarmStorage::dealCall { id }).endEpoch, 25);
    n.evm.call(n.owner, SWARM, U256::ZERO, ISwarmStorage::cancelDealCall { id });
    assert_eq!(n.evm.view(SWARM, ISwarmStorage::dealCall { id }).endEpoch, 13);
    set_epoch(&mut n.evm, 13);
    n.evm.call(n.owner, SWARM, U256::ZERO, ISwarmStorage::closeDealCall { id });
    // Paid: epoch 12 to the replacement; the leaver forfeited 10 and 11. Refund: 15 - 1.
    assert_eq!(n.in_contract(n.owner), per_epoch() * U256::from(14));
    let o = n.evm.view(SWARM, ISwarmStorage::offerCall { id: s.providers[0] });
    assert_eq!((o.usedMiB, o.committed), (0, U256::ZERO));
}

#[test]
fn validators_offer_storage_without_a_bond_limit() {
    let mut n = net(0);
    let g = crate::tests::dev_genesis();
    let owner = g.dev_validators[0].owner;
    n.evm.fund(owner, bolt(1));
    n.evm.call(
        owner,
        SWARM,
        U256::ZERO,
        ISwarmStorage::offerStorageCall { id: 1, capacityMiB: 1 << 20, minPrice: 1 },
    );
    let c = ISwarmStorage::createDealCall {
        root: Bytes::from_static(b"r"),
        blocks: 1,
        size: 1024 * 1024 * 1024,
        replicas: 1,
        epochs: 100,
        price: GIB_PRICE,
    };
    let id = n.evm.call(n.owner, SWARM, bolt(100), c);
    assert_eq!(n.slots(id).providers, vec![1]);
    set_epoch(&mut n.evm, 12);
    n.evm.call(owner, SWARM, U256::ZERO, ISwarmStorage::claimCall { id, slot: U256::ZERO });
    assert_eq!(n.in_contract(owner), bolt(2), "validators are paid in full");
}

/// The public testnet switches from HistoryRegistry to SwarmStorage by replacing the code at the
/// `swarm` fork: everything recorded before must read the same afterwards (storage layout), and
/// the new functions must work on top of it.
#[test]
fn the_testnet_fork_keeps_history_state() {
    use bolt_primitives::forks::TESTNET_SWARM_STORAGE;
    const OLD: &[u8] = include_bytes!("../../../contracts/forks/8018-genesis/HistoryRegistry.bin");
    fn set_code(evm: &mut Evm, code: &[u8]) {
        let mut info = evm.db.load_account(SWARM).unwrap().info.clone();
        let c = revm::bytecode::Bytecode::new_raw(Bytes::copy_from_slice(code));
        info.code_hash = c.hash_slow();
        info.code = Some(c);
        evm.db.insert_account_info(SWARM, info);
    }
    let mut evm = Evm::from_genesis(&crate::tests::dev_genesis());
    evm.timestamp = evm.timestamp.max(1_000_000);
    set_code(&mut evm, OLD);
    set_epoch(&mut evm, 10);

    // Under HistoryRegistry: an epoch index, a storage provider with a bond, a decided audit.
    let cid = Bytes::from_static(b"epoch-0-index");
    evm.system(SWARM, ISwarmStorage::recordEpochCall { epoch: 0, cid: cid.clone() });
    let account = Address::repeat_byte(0x61);
    evm.fund(account, bolt(1));
    let id = evm.call(
        account,
        SWARM,
        U256::ZERO,
        ISwarmStorage::registerStorageCall { peerId: Bytes::from_static(b"peer") },
    );
    evm.fund(REWARDS, bolt(10));
    evm.system(
        SWARM,
        ISwarmStorage::beginAuditsCall {
            epoch: 10,
            panel: vec![1],
            providers: vec![id],
            targets: vec![0],
            heights: vec![3],
        },
    );
    assert!(evm.system(SWARM, ISwarmStorage::recordAuditCall { epoch: 10, task: 0, ok: true }));
    evm.system(REWARDS, IRewardDistributor::settleStorageCall { epoch: 10, emission: bolt(10) });
    evm.call(account, REWARDS, U256::ZERO, IRewardDistributor::claimCall { id });
    let bond = evm.view(SWARM, ISwarmStorage::storageProviderCall { id }).bond;
    assert_eq!(bond, bolt(2));
    assert_eq!(evm.balance(SWARM), bolt(2));
    // The old contract knows nothing of deals.
    assert!(!evm.try_call(account, SWARM, U256::ZERO, ISwarmStorage::dealCountCall {}));

    // The fork: new code, same storage and balance.
    set_code(&mut evm, TESTNET_SWARM_STORAGE);
    assert_eq!(evm.view(SWARM, ISwarmStorage::epochIndexCall { epoch: 0 }), cid);
    assert_eq!(evm.view(SWARM, ISwarmStorage::indexedEpochsCall {}), 1);
    let p = evm.view(SWARM, ISwarmStorage::storageProviderCall { id });
    assert_eq!((p.account, p.active, p.bond), (account, true, bolt(2)));
    assert_eq!(evm.view(SWARM, ISwarmStorage::peerOfCall { id }), Bytes::from_static(b"peer"));
    let a = evm.view(SWARM, ISwarmStorage::auditsCall { epoch: 10 });
    assert_eq!((a.providers, a.heights, a.states), (vec![id], vec![3], vec![1u8]));
    assert_eq!(evm.view(SWARM, ISwarmStorage::taskKindsCall { epoch: 10 }), vec![0u8]);
    assert_eq!(evm.view(SWARM, ISwarmStorage::passedCall { epoch: 10 }), vec![id]);
    assert_eq!(evm.balance(SWARM), bolt(2));
    assert_eq!(evm.view(SWARM, ISwarmStorage::dealCountCall {}), U256::ZERO);

    // Deals work on top.
    evm.call(
        account,
        SWARM,
        U256::ZERO,
        ISwarmStorage::offerStorageCall { id, capacityMiB: 64, minPrice: 1 },
    );
    let owner = Address::repeat_byte(0x62);
    evm.fund(owner, bolt(10));
    let d = evm.call(
        owner,
        SWARM,
        bolt(1),
        ISwarmStorage::createDealCall {
            root: Bytes::from_static(b"r"),
            blocks: 4,
            size: 1 << 20,
            replicas: 1,
            epochs: 5,
            price: GIB_PRICE,
        },
    );
    assert_eq!(evm.view(SWARM, ISwarmStorage::dealSlotsCall { id: d }).providers, vec![id]);
}

/// The fork installs exactly the SwarmStorage this source tree compiles to (until the fork is
/// live; after that the pinned copy must never change).
#[test]
fn pinned_testnet_swarm_code_is_current() {
    assert_eq!(
        bolt_primitives::forks::TESTNET_SWARM_STORAGE,
        crate::artifacts::swarm_storage().deployed.as_ref()
    );
}
