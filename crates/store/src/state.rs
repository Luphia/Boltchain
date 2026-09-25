//! Applying execution results to the flat state and tries, recent history, and the revm
//! database view.

use crate::{
    HISTORY_BLOCKS,
    db::{
        Account, AccountNodes, Result, StorageNodes, StoreError, Tx, encode_hist_info,
        hashed_address, num_key, storage_key, t,
    },
    trie::Trie,
};
use alloy_primitives::{Address, B256, U256, keccak256};
use alloy_trie::{EMPTY_ROOT_HASH, KECCAK_EMPTY};
use libmdbx::{RW, TransactionKind};
use revm::{
    DatabaseRef,
    bytecode::Bytecode,
    database::{
        BundleState, OriginalValuesKnown, RevertToSlot,
        states::{PlainStateReverts, StateChangeset},
    },
    database_interface::DBErrorMarker,
    state::AccountInfo,
};
use std::collections::BTreeMap;

impl DBErrorMarker for StoreError {}

fn trie_err<E: std::fmt::Debug>(e: crate::trie::TrieError<E>) -> StoreError {
    StoreError::Trie(format!("{e:?}"))
}

/// A predeployed or genesis account.
#[derive(Debug, Clone, Default)]
pub struct InitAccount {
    /// Nonce.
    pub nonce: u64,
    /// Balance.
    pub balance: U256,
    /// Code.
    pub code: alloy_primitives::Bytes,
    /// Storage.
    pub storage: BTreeMap<B256, U256>,
}

impl Tx<'_, RW> {
    /// Writes genesis state and returns the state root.
    pub fn init_state(&self, alloc: &BTreeMap<Address, InitAccount>) -> Result<B256> {
        let mut changes = StateChangeset::default();
        for (addr, acc) in alloc {
            let code_hash = if acc.code.is_empty() {
                KECCAK_EMPTY
            } else {
                let code = Bytecode::new_raw(acc.code.clone());
                let h = code.hash_slow();
                changes.contracts.push((h, code));
                h
            };
            changes.accounts.push((
                *addr,
                Some(AccountInfo {
                    balance: acc.balance,
                    nonce: acc.nonce,
                    code_hash,
                    ..Default::default()
                }),
            ));
            changes.storage.push(revm::database::states::PlainStorageChangeset {
                address: *addr,
                wipe_storage: false,
                storage: acc.storage.iter().map(|(k, v)| (U256::from_be_bytes(k.0), *v)).collect(),
            });
        }
        self.apply_changes(&changes)
    }

    /// Applies one block's execution result: flat state, tries and history. Returns the new
    /// state root. The bundle must contain exactly this block's transitions and reverts.
    pub fn apply_bundle(&self, number: u64, bundle: BundleState) -> Result<B256> {
        let (changes, reverts) = bundle.to_plain_state_and_reverts(OriginalValuesKnown::Yes);
        self.write_history(number, &reverts)?;
        self.apply_changes(&changes)
    }

    pub(crate) fn apply_changes(&self, changes: &StateChangeset) -> Result<B256> {
        for (hash, code) in &changes.contracts {
            self.put_raw(t::CODES, hash.as_slice(), code.original_byte_slice())?;
        }

        // 1. Storage: flat slots and per-account storage tries.
        let mut storage_roots: BTreeMap<Address, B256> = BTreeMap::new();
        for s in &changes.storage {
            let hashed = hashed_address(&s.address);
            if s.wipe_storage {
                self.del_prefix(t::STORAGE, s.address.as_slice())?;
                self.del_prefix(t::TRIE_STO, hashed.as_slice())?;
            }
            if s.storage.is_empty() {
                if s.wipe_storage {
                    storage_roots.insert(s.address, EMPTY_ROOT_HASH);
                }
                continue;
            }
            let nodes = StorageNodes { tx: self, hashed };
            let mut trie = Trie::new(&nodes);
            for (slot, value) in &s.storage {
                let slot = B256::from(*slot);
                let key = storage_key(&s.address, &slot);
                let hkey = keccak256(slot);
                if value.is_zero() {
                    self.del_raw(t::STORAGE, &key)?;
                    trie.remove_key(&hkey).map_err(trie_err)?;
                } else {
                    self.put_raw(t::STORAGE, &key, &value.to_be_bytes::<32>())?;
                    trie.insert(&hkey, alloy_rlp::encode(value)).map_err(trie_err)?;
                }
            }
            let (root, writes) = trie.commit().map_err(trie_err)?;
            self.apply_node_writes(t::TRIE_STO, hashed.as_slice(), writes)?;
            storage_roots.insert(s.address, root);
        }

        // 2. Accounts, including those whose only change was storage.
        let mut infos: BTreeMap<Address, Option<AccountInfo>> =
            changes.accounts.iter().cloned().collect();
        for addr in storage_roots.keys() {
            if !infos.contains_key(addr) {
                let acc = self.account(addr)?.unwrap_or_default();
                infos.insert(
                    *addr,
                    Some(AccountInfo {
                        balance: acc.balance,
                        nonce: acc.nonce,
                        code_hash: acc.code_hash,
                        ..Default::default()
                    }),
                );
            }
        }

        let nodes = AccountNodes(self);
        let mut trie = Trie::new(&nodes);
        for (addr, info) in &infos {
            let hkey = hashed_address(addr);
            match info {
                None => {
                    self.del_raw(t::ACCOUNTS, addr.as_slice())?;
                    self.del_prefix(t::STORAGE, addr.as_slice())?;
                    self.del_prefix(t::TRIE_STO, hkey.as_slice())?;
                    trie.remove_key(&hkey).map_err(trie_err)?;
                }
                Some(info) => {
                    let storage_root = match storage_roots.get(addr) {
                        Some(r) => *r,
                        None => {
                            self.account(addr)?.map(|a| a.storage_root).unwrap_or(EMPTY_ROOT_HASH)
                        }
                    };
                    let acc = Account {
                        nonce: info.nonce,
                        balance: info.balance,
                        code_hash: info.code_hash,
                        storage_root,
                    };
                    self.put_raw(t::ACCOUNTS, addr.as_slice(), &acc.encode())?;
                    trie.insert(&hkey, acc.trie_value()).map_err(trie_err)?;
                }
            }
        }
        let (root, writes) = trie.commit().map_err(trie_err)?;
        self.apply_node_writes(t::TRIE_ACC, &[], writes)?;
        Ok(root)
    }

    /// Records prior values so state as of recent blocks can be served.
    fn write_history(&self, number: u64, reverts: &PlainStateReverts) -> Result<()> {
        let n = num_key(number);
        for (addr, prior) in reverts.accounts.iter().flatten() {
            let info = prior.as_ref().map(|i| (i.nonce, i.balance, i.code_hash));
            self.put_raw(t::ACC_HIST, &[addr.as_slice(), &n].concat(), &encode_hist_info(info))?;
            self.put_raw(t::HIST_KEYS, &[&n[..], &[0u8], addr.as_slice()].concat(), &[])?;
        }
        for s in reverts.storage.iter().flatten() {
            for (slot, prior) in &s.storage_revert {
                // Since EIP-6780 storage can only be wiped for contracts created in the same
                // transaction, so a wiped slot was empty before the block.
                let value = match prior {
                    RevertToSlot::Some(v) => *v,
                    RevertToSlot::Destroyed => U256::ZERO,
                };
                let key = storage_key(&s.address, &B256::from(*slot));
                self.put_raw(t::STO_HIST, &[&key[..], &n].concat(), &value.to_be_bytes::<32>())?;
                self.put_raw(t::HIST_KEYS, &[&n[..], &[1u8], &key[..]].concat(), &[])?;
            }
        }
        Ok(())
    }

    /// Undoes the head block: restores the state before it from the recorded history, removes
    /// the block and moves the head to its parent. Returns the removed block and the restored
    /// state root (which must equal the parent's). Only the last [`HISTORY_BLOCKS`] blocks can be
    /// undone.
    pub fn unwind_head(&self) -> Result<(crate::StoredBlock, B256)> {
        let number = self.head()?.ok_or(StoreError::Corrupt("unwind: empty chain"))?;
        if number == 0 {
            return Err(StoreError::Corrupt("unwind: cannot undo genesis"));
        }
        let root = self.unwind_state(number)?;
        let block = self.remove_head_block(number)?;
        Ok((block, root))
    }

    /// Applies the history recorded for block `number` in reverse and deletes it.
    fn unwind_state(&self, number: u64) -> Result<B256> {
        let n = num_key(number);
        let tbl = self.table(t::HIST_KEYS)?;
        let mut cursor = self.txn.cursor(&tbl)?;
        let mut keys = Vec::new();
        let mut item = cursor.set_range::<Vec<u8>, ()>(&n)?;
        while let Some((k, ())) = item {
            if !k.starts_with(&n) {
                break;
            }
            keys.push(k);
            item = cursor.next::<Vec<u8>, ()>()?;
        }
        drop(cursor);
        if keys.is_empty() && !self.history_covers(number)? {
            return Err(StoreError::Corrupt("unwind: history pruned"));
        }
        let mut changes = StateChangeset::default();
        let mut storage: BTreeMap<Address, Vec<(U256, U256)>> = BTreeMap::new();
        for k in &keys {
            let entry = &k[9..];
            let hist_key = [entry, &n[..]].concat();
            if k[8] == 0 {
                let addr = Address::from_slice(entry);
                let raw = self
                    .get_raw(t::ACC_HIST, &hist_key)?
                    .ok_or(StoreError::Corrupt(t::ACC_HIST))?;
                let prior =
                    crate::db::decode_hist_info(&raw).ok_or(StoreError::Corrupt(t::ACC_HIST))?;
                let info = prior.map(|(nonce, balance, code_hash)| AccountInfo {
                    nonce,
                    balance,
                    code_hash,
                    ..Default::default()
                });
                changes.accounts.push((addr, info));
                self.del_raw(t::ACC_HIST, &hist_key)?;
            } else {
                let addr = Address::from_slice(&entry[..20]);
                let slot = U256::from_be_slice(&entry[20..52]);
                let raw = self
                    .get_raw(t::STO_HIST, &hist_key)?
                    .ok_or(StoreError::Corrupt(t::STO_HIST))?;
                storage.entry(addr).or_default().push((slot, U256::from_be_slice(&raw)));
                self.del_raw(t::STO_HIST, &hist_key)?;
            }
            self.del_raw(t::HIST_KEYS, k)?;
        }
        for (address, slots) in storage {
            changes.storage.push(revm::database::states::PlainStorageChangeset {
                address,
                wipe_storage: false,
                storage: slots,
            });
        }
        self.apply_changes(&changes)
    }

    /// Drops history older than [`HISTORY_BLOCKS`] behind `head`.
    pub fn prune_history(&self, head: u64) -> Result<usize> {
        let Some(cutoff) = head.checked_sub(HISTORY_BLOCKS) else { return Ok(0) };
        let tbl = self.table(t::HIST_KEYS)?;
        let mut cursor = self.txn.cursor(&tbl)?;
        let mut doomed = Vec::new();
        let mut item = cursor.first::<Vec<u8>, ()>()?;
        while let Some((k, ())) = item {
            let num = u64::from_be_bytes(k[..8].try_into().unwrap_or_default());
            if num > cutoff {
                break;
            }
            doomed.push(k);
            item = cursor.next::<Vec<u8>, ()>()?;
        }
        drop(cursor);
        for k in &doomed {
            let n = &k[..8];
            let entry = &k[9..];
            let table = if k[8] == 0 { t::ACC_HIST } else { t::STO_HIST };
            self.del_raw(table, &[entry, n].concat())?;
            self.del_raw(t::HIST_KEYS, k)?;
        }
        Ok(doomed.len())
    }
}

/// Collects the set of addresses touched by the history table (test helper).
#[doc(hidden)]
pub fn history_len<K: TransactionKind>(tx: &Tx<'_, K>) -> Result<usize> {
    let tbl = tx.table(t::HIST_KEYS)?;
    let mut cursor = tx.txn.cursor(&tbl)?;
    let mut n = 0;
    let mut item = cursor.first::<Vec<u8>, ()>()?;
    while item.is_some() {
        n += 1;
        item = cursor.next::<Vec<u8>, ()>()?;
    }
    Ok(n)
}

/// revm view of the state, either latest or as of a recent block.
#[derive(Debug)]
pub struct StateView<'a, 'e, K: TransactionKind> {
    tx: &'a Tx<'e, K>,
    at: Option<u64>,
}

impl<'a, 'e, K: TransactionKind> StateView<'a, 'e, K> {
    /// Latest state.
    pub fn latest(tx: &'a Tx<'e, K>) -> Self {
        Self { tx, at: None }
    }

    /// State as of the end of block `number`; fails if history no longer covers it.
    pub fn at(tx: &'a Tx<'e, K>, number: u64) -> Result<Self> {
        if !tx.history_covers(number)? {
            return Err(StoreError::Corrupt("history pruned"));
        }
        Ok(Self { tx, at: Some(number) })
    }
}

impl<K: TransactionKind> DatabaseRef for StateView<'_, '_, K> {
    type Error = StoreError;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>> {
        let acc = match self.at {
            None => self.tx.account(&address)?,
            Some(n) => self.tx.account_at(&address, n)?.ok_or(StoreError::Corrupt("history"))?,
        };
        // `code: None` makes revm fetch the code through `code_by_hash_ref`. (The default
        // `AccountInfo` carries `Some(empty code)`, which would silently skip contract code.)
        Ok(acc.map(|a| AccountInfo {
            balance: a.balance,
            nonce: a.nonce,
            code_hash: a.code_hash,
            code: None,
            ..Default::default()
        }))
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode> {
        if code_hash == KECCAK_EMPTY {
            return Ok(Bytecode::default());
        }
        let raw = self.tx.code(&code_hash)?.ok_or(StoreError::Corrupt(t::CODES))?;
        Ok(Bytecode::new_raw(raw))
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256> {
        let slot = B256::from(index);
        match self.at {
            None => self.tx.storage(&address, &slot),
            Some(n) => {
                self.tx.storage_at(&address, &slot, n)?.ok_or(StoreError::Corrupt("history"))
            }
        }
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256> {
        Ok(self.tx.block_hash(number)?.unwrap_or_default())
    }
}

/// Recomputes the state root from the flat tables alone (slow; for tests and audits).
pub fn full_state_root<K: TransactionKind>(tx: &Tx<'_, K>) -> Result<B256> {
    let mut storage: BTreeMap<Address, Vec<(B256, U256)>> = BTreeMap::new();
    {
        let tbl = tx.table(t::STORAGE)?;
        let mut c = tx.txn.cursor(&tbl)?;
        let mut item = c.first::<Vec<u8>, Vec<u8>>()?;
        while let Some((k, v)) = item {
            storage
                .entry(Address::from_slice(&k[..20]))
                .or_default()
                .push((B256::from_slice(&k[20..]), U256::from_be_slice(&v)));
            item = c.next()?;
        }
    }
    let mut accounts = Vec::new();
    let tbl = tx.table(t::ACCOUNTS)?;
    let mut c = tx.txn.cursor(&tbl)?;
    let mut item = c.first::<Vec<u8>, Vec<u8>>()?;
    while let Some((k, v)) = item {
        let addr = Address::from_slice(&k);
        let a = Account::decode(&v).ok_or(StoreError::Corrupt(t::ACCOUNTS))?;
        let root = alloy_trie::root::storage_root_unhashed(
            storage.get(&addr).cloned().unwrap_or_default(),
        );
        accounts.push((
            addr,
            alloy_trie::TrieAccount {
                nonce: a.nonce,
                balance: a.balance,
                storage_root: root,
                code_hash: a.code_hash,
            },
        ));
        item = c.next()?;
    }
    Ok(alloy_trie::root::state_root_unhashed(accounts))
}
