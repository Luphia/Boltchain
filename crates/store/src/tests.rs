use crate::{db::Account, state::history_len, *};
use alloy_primitives::{Address, B256, Bytes, U256};
use proptest::prelude::*;
use revm::{
    database::states::{PlainStorageChangeset, StateChangeset},
    state::AccountInfo,
};
use std::collections::BTreeMap;

fn open() -> (tempfile::TempDir, Store) {
    let dir = tempfile::tempdir().unwrap();
    let store = Store::open(dir.path()).unwrap();
    (dir, store)
}

#[test]
fn genesis_state_root_matches_primitives() {
    // The genesis root computed by bolt-primitives (pinned against a Python implementation)
    // must equal the root the store produces from the same alloc.
    let (_d, store) = open();
    let mut alloc = BTreeMap::new();
    let code = Bytes::from_static(&[0x60, 0x00, 0x60, 0x00, 0xf3]);
    let mut storage = BTreeMap::new();
    storage.insert(B256::with_last_byte(1), U256::from(42));
    alloc.insert(
        Address::repeat_byte(1),
        InitAccount { nonce: 1, balance: U256::from(5), code: code.clone(), storage },
    );
    alloc.insert(
        Address::repeat_byte(2),
        InitAccount { balance: U256::from(9), ..Default::default() },
    );
    let w = store.writer().unwrap();
    let root = w.init_state(&alloc).unwrap();
    assert_eq!(root, full_state_root(&w).unwrap());

    let expected = alloy_trie::root::state_root_unhashed(alloc.iter().map(|(a, acc)| {
        (
            *a,
            alloy_trie::TrieAccount {
                nonce: acc.nonce,
                balance: acc.balance,
                storage_root: alloy_trie::root::storage_root_unhashed(
                    acc.storage.iter().map(|(k, v)| (*k, *v)),
                ),
                code_hash: if acc.code.is_empty() {
                    alloy_trie::KECCAK_EMPTY
                } else {
                    alloy_primitives::keccak256(&acc.code)
                },
            },
        )
    }));
    assert_eq!(root, expected);
    w.commit().unwrap();
    let r = store.reader().unwrap();
    assert_eq!(
        r.storage(&Address::repeat_byte(1), &B256::with_last_byte(1)).unwrap(),
        U256::from(42)
    );
}

#[derive(Debug, Clone)]
enum Op {
    SetAccount(u8, u64, u64),
    DeleteAccount(u8),
    SetSlot(u8, u8, u64),
    Wipe(u8),
}

fn op() -> impl Strategy<Value = Op> {
    prop_oneof![
        4 => (0u8..12, 0u64..5, 0u64..1000).prop_map(|(a, n, b)| Op::SetAccount(a, n, b)),
        1 => (0u8..12).prop_map(Op::DeleteAccount),
        6 => (0u8..12, 0u8..20, 0u64..50).prop_map(|(a, s, v)| Op::SetSlot(a, s, v)),
        1 => (0u8..12).prop_map(Op::Wipe),
    ]
}

fn to_changeset(ops: &[Op], existing: &BTreeMap<u8, Account>) -> StateChangeset {
    let mut cs = StateChangeset::default();
    let mut storage: BTreeMap<u8, PlainStorageChangeset> = BTreeMap::new();
    for o in ops {
        match o {
            Op::SetAccount(a, n, b) => cs.accounts.push((
                Address::repeat_byte(*a),
                Some(AccountInfo { nonce: *n, balance: U256::from(*b), ..Default::default() }),
            )),
            Op::DeleteAccount(a) => {
                cs.accounts.push((Address::repeat_byte(*a), None));
                storage.remove(a);
            }
            Op::SetSlot(a, s, v) => {
                if !existing.contains_key(a)
                    && !cs
                        .accounts
                        .iter()
                        .any(|(x, i)| *x == Address::repeat_byte(*a) && i.is_some())
                {
                    continue; // storage only on existing accounts
                }
                storage
                    .entry(*a)
                    .or_insert_with(|| PlainStorageChangeset {
                        address: Address::repeat_byte(*a),
                        ..Default::default()
                    })
                    .storage
                    .push((U256::from(*s), U256::from(*v)));
            }
            Op::Wipe(a) => {
                let e = storage.entry(*a).or_insert_with(|| PlainStorageChangeset {
                    address: Address::repeat_byte(*a),
                    ..Default::default()
                });
                e.wipe_storage = true;
                e.storage.clear();
            }
        }
    }
    // Last write per account wins, like revm's changeset.
    let mut last: BTreeMap<Address, Option<AccountInfo>> = BTreeMap::new();
    for (a, i) in cs.accounts.drain(..) {
        last.insert(a, i);
    }
    // Deleted accounts carry no storage changes.
    for (a, i) in &last {
        if i.is_none() {
            storage.retain(|_, s| s.address != *a);
        }
    }
    cs.accounts = last.into_iter().collect();
    for mut s in storage.into_values() {
        let mut m = BTreeMap::new();
        for (k, v) in s.storage.drain(..) {
            m.insert(k, v);
        }
        s.storage = m.into_iter().collect();
        cs.storage.push(s);
    }
    cs
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(64))]

    /// After any sequence of blocks, the incrementally maintained root equals a full
    /// recomputation from the flat tables.
    #[test]
    fn incremental_state_root_matches_full(blocks in prop::collection::vec(prop::collection::vec(op(), 0..25), 1..6)) {
        let (_d, store) = open();
        let mut existing: BTreeMap<u8, Account> = BTreeMap::new();
        for ops in &blocks {
            let cs = to_changeset(ops, &existing);
            let w = store.writer().unwrap();
            let root = w.apply_changes(&cs).unwrap();
            prop_assert_eq!(root, full_state_root(&w).unwrap());
            existing.clear();
            for i in 0u8..12 {
                if let Some(a) = w.account(&Address::repeat_byte(i)).unwrap() {
                    existing.insert(i, a);
                }
            }
            w.commit().unwrap();
        }
    }
}

#[test]
fn history_serves_recent_state_and_prunes() {
    use revm::database::{
        BundleState,
        states::reverts::{AccountInfoRevert, AccountRevert},
    };
    let _ = (AccountInfoRevert::DoNothing, AccountRevert::default());

    let (_d, store) = open();
    let alice = Address::repeat_byte(0xa1);
    // Build 200 blocks, each setting alice's balance to the block number.
    let mut prev: Option<AccountInfo> = None;
    for n in 1..=200u64 {
        let info = AccountInfo { balance: U256::from(n), ..Default::default() };
        let mut b = BundleState::builder(n..=n);
        b = b.state_present_account_info(alice, info.clone());
        if let Some(p) = &prev {
            b = b.state_original_account_info(alice, p.clone());
            b = b.revert_account_info(n, alice, Some(Some(p.clone())));
        } else {
            b = b.revert_account_info(n, alice, Some(None));
        }
        let bundle = b.build();
        let w = store.writer().unwrap();
        w.apply_bundle(n, bundle).unwrap();
        let header = alloy_consensus::Header { number: n, ..Default::default() };
        w.put_block(&StoredBlock { header, transactions: vec![], senders: vec![] }, &[]).unwrap();
        w.prune_history(n).unwrap();
        w.commit().unwrap();
        prev = Some(info);
    }
    let r = store.reader().unwrap();
    assert_eq!(r.head().unwrap(), Some(200));
    assert_eq!(r.account(&alice).unwrap().unwrap().balance, U256::from(200));
    // Within the window: exact historical balances.
    for n in [72u64, 100, 150, 199, 200] {
        let a = r.account_at(&alice, n).unwrap().expect("covered").unwrap();
        assert_eq!(a.balance, U256::from(n), "balance at block {n}");
    }
    // Outside the window.
    assert!(r.account_at(&alice, 71).unwrap().is_none());
    // Only the last 128 blocks of history remain.
    assert_eq!(history_len(&r).unwrap(), 128);
}

#[test]
fn unwind_restores_state_blocks_and_total_difficulty() {
    use revm::database::BundleState;
    let (_d, store) = open();
    let alice = Address::repeat_byte(0xa1);
    let contract = Address::repeat_byte(0xc0);
    let w = store.writer().unwrap();
    let mut alloc = BTreeMap::new();
    alloc.insert(alice, InitAccount { balance: U256::from(1000), ..Default::default() });
    let genesis_root = w.init_state(&alloc).unwrap();
    let header = alloy_consensus::Header {
        number: 0,
        state_root: genesis_root,
        difficulty: U256::from(7),
        ..Default::default()
    };
    w.put_block(&StoredBlock { header, transactions: vec![], senders: vec![] }, &[]).unwrap();
    w.commit().unwrap();

    let mut roots = vec![genesis_root];
    let mut alice_info = AccountInfo { balance: U256::from(1000), ..Default::default() };
    let mut slot_value = U256::ZERO;
    for n in 1..=5u64 {
        let new_alice =
            AccountInfo { balance: U256::from(1000 + n), nonce: n, ..Default::default() };
        let mut b = BundleState::builder(n..=n)
            .state_original_account_info(alice, alice_info.clone())
            .state_present_account_info(alice, new_alice.clone())
            .revert_account_info(n, alice, Some(Some(alice_info.clone())));
        let new_slot = U256::from(n * 10);
        let contract_info = AccountInfo { nonce: 1, ..Default::default() };
        if n == 2 {
            // created in block 2
            b = b.state_present_account_info(contract, contract_info.clone()).revert_account_info(
                n,
                contract,
                Some(None),
            );
        } else if n > 2 {
            b = b
                .state_original_account_info(contract, contract_info.clone())
                .state_present_account_info(contract, contract_info.clone());
        }
        if n >= 2 {
            b = b
                .state_storage(
                    contract,
                    [(U256::from(1), (slot_value, new_slot))].into_iter().collect(),
                )
                .revert_storage(n, contract, vec![(U256::from(1), slot_value)]);
        }
        let w = store.writer().unwrap();
        let root = w.apply_bundle(n, b.build()).unwrap();
        assert_eq!(root, full_state_root(&w).unwrap());
        let header = alloy_consensus::Header {
            number: n,
            state_root: root,
            difficulty: U256::from(n),
            ..Default::default()
        };
        w.put_block(&StoredBlock { header, transactions: vec![], senders: vec![] }, &[]).unwrap();
        w.commit().unwrap();
        roots.push(root);
        alice_info = new_alice;
        if n >= 2 {
            slot_value = new_slot;
        }
    }
    let r = store.reader().unwrap();
    assert_eq!(r.total_difficulty(5).unwrap(), Some(U256::from(7 + 1 + 2 + 3 + 4 + 5)));
    drop(r);

    for n in (1..=5u64).rev() {
        let w = store.writer().unwrap();
        let (block, root) = w.unwind_head().unwrap();
        assert_eq!(block.header.number, n);
        assert_eq!(root, roots[n as usize - 1], "root after undoing block {n}");
        assert_eq!(root, full_state_root(&w).unwrap());
        w.commit().unwrap();
        let r = store.reader().unwrap();
        assert_eq!(r.head().unwrap(), Some(n - 1));
        assert!(r.header(n).unwrap().is_none());
        assert!(r.total_difficulty(n).unwrap().is_none());
    }
    let r = store.reader().unwrap();
    assert_eq!(r.account(&alice).unwrap().unwrap().balance, U256::from(1000));
    assert!(r.account(&contract).unwrap().is_none());
    assert_eq!(r.storage(&contract, &B256::with_last_byte(1)).unwrap(), U256::ZERO);
    assert_eq!(history_len(&r).unwrap(), 0);
    let w = store.writer().unwrap();
    assert!(w.unwind_head().is_err(), "genesis cannot be undone");
}
