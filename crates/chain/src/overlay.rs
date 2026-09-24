//! State of not-yet-final blocks layered over the committed store.

use alloy_primitives::{Address, B256, U256};
use bolt_store::{RO, StateView, StoreError};
use revm::{DatabaseRef, bytecode::Bytecode, database::states::StateChangeset, state::AccountInfo};
use std::collections::{HashMap, HashSet};

/// Changes of pending blocks, applied oldest first, on top of the committed state.
#[derive(Debug, Default, Clone)]
pub struct Changes {
    accounts: HashMap<Address, Option<AccountInfo>>,
    storage: HashMap<(Address, U256), U256>,
    wiped: HashSet<Address>,
    codes: HashMap<B256, Bytecode>,
    block_hashes: HashMap<u64, B256>,
}

impl Changes {
    /// Adds one block's changes (and its hash, for BLOCKHASH).
    pub fn push(&mut self, number: u64, hash: B256, cs: &StateChangeset) {
        for (h, code) in &cs.contracts {
            self.codes.insert(*h, code.clone());
        }
        for s in &cs.storage {
            if s.wipe_storage {
                self.wiped.insert(s.address);
                self.storage.retain(|(a, _), _| *a != s.address);
            }
            for (k, v) in &s.storage {
                self.storage.insert((s.address, *k), *v);
            }
        }
        for (a, info) in &cs.accounts {
            if info.is_none() {
                self.wiped.insert(*a);
                self.storage.retain(|(x, _), _| x != a);
            }
            self.accounts.insert(
                *a,
                info.clone().map(|mut i| {
                    i.code = None;
                    i
                }),
            );
        }
        self.block_hashes.insert(number, hash);
    }
}

/// revm database: pending changes over a committed-state view.
#[derive(Debug)]
pub struct Overlay<'a, 'b, 'e> {
    /// Committed state.
    pub base: &'a StateView<'b, 'e, RO>,
    /// Pending changes.
    pub changes: &'a Changes,
}

impl DatabaseRef for Overlay<'_, '_, '_> {
    type Error = StoreError;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, StoreError> {
        match self.changes.accounts.get(&address) {
            Some(info) => Ok(info.clone()),
            None => self.base.basic_ref(address),
        }
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, StoreError> {
        match self.changes.codes.get(&code_hash) {
            Some(c) => Ok(c.clone()),
            None => self.base.code_by_hash_ref(code_hash),
        }
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, StoreError> {
        if let Some(v) = self.changes.storage.get(&(address, index)) {
            return Ok(*v);
        }
        if self.changes.wiped.contains(&address) {
            return Ok(U256::ZERO);
        }
        self.base.storage_ref(address, index)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, StoreError> {
        match self.changes.block_hashes.get(&number) {
            Some(h) => Ok(*h),
            None => self.base.block_hash_ref(number),
        }
    }
}
