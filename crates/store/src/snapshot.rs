//! State snapshots (ADR 0009): the flat state of one block as a deterministic IPLD DAG, and the
//! reverse (rebuilding state and tries from one), plus pruning of old block data.

use crate::{
    Account, Result, StoreError, StoredBlock, Tx,
    db::{META_BASE, num_key, t},
};
use alloy_consensus::Header;
use alloy_primitives::{Address, B256, U256};
use bolt_ipld::{
    Cid,
    history::{SnapshotBuilder, SnapshotRoot, decode_accounts, decode_codes},
};
use libmdbx::{RW, TransactionKind};
use revm::{
    bytecode::Bytecode,
    database::states::{PlainStorageChangeset, StateChangeset},
    state::AccountInfo,
};
use std::collections::BTreeSet;

const META_SNAPSHOTS: &[u8] = b"snapshots";

/// Builds the snapshot of the state in `tx` (which must be the post-state of `header`), passing
/// every IPFS block to `emit` as it is produced. Returns the root CID.
pub fn build_snapshot<K: TransactionKind>(
    tx: &Tx<'_, K>,
    chain_id: u64,
    header: &Header,
    envelope: Cid,
    td: U256,
    mut emit: impl FnMut(Cid, Vec<u8>) -> Result<()>,
) -> Result<Cid> {
    let mut b = SnapshotBuilder::default();
    let mut codes = BTreeSet::new();
    {
        let acc = tx.table(t::ACCOUNTS)?;
        let sto = tx.table(t::STORAGE)?;
        let mut ac = tx.txn.cursor(&acc)?;
        let mut sc = tx.txn.cursor(&sto)?;
        let mut item = ac.first::<Vec<u8>, Vec<u8>>()?;
        while let Some((k, v)) = item {
            let a = Account::decode(&v).ok_or(StoreError::Corrupt(t::ACCOUNTS))?;
            if a.code_hash != alloy_trie::KECCAK_EMPTY {
                codes.insert(a.code_hash);
            }
            let mut slots = Vec::new();
            let mut s = sc.set_range::<Vec<u8>, Vec<u8>>(&k)?;
            while let Some((sk, sv)) = s {
                if !sk.starts_with(&k) {
                    break;
                }
                let mut slot = [0u8; 32];
                slot.copy_from_slice(&sk[20..52]);
                let mut val = [0u8; 32];
                val[32 - sv.len()..].copy_from_slice(&sv);
                slots.push((slot, val));
                s = sc.next()?;
            }
            let mut addr = [0u8; 20];
            addr.copy_from_slice(&k);
            for (c, bytes) in
                b.account(addr, a.nonce, a.balance.to_be_bytes::<32>(), a.code_hash.0, slots)
            {
                emit(c, bytes)?;
            }
            item = ac.next()?;
        }
    }
    for h in &codes {
        let code = tx.code(h)?.ok_or(StoreError::Corrupt(t::CODES))?;
        for (c, bytes) in b.code(&code) {
            emit(c, bytes)?;
        }
    }
    for (c, bytes) in b.finish() {
        emit(c, bytes)?;
    }
    let root = SnapshotRoot {
        v: 1,
        chain_id,
        number: header.number,
        hash: header.hash_slow().to_vec().into(),
        state_root: header.state_root.to_vec().into(),
        td: td.to_be_bytes::<32>().to_vec().into(),
        envelope,
        accounts: b.accounts.clone(),
        codes: b.code_chunks.clone(),
    };
    let (cid, bytes) = root.to_block();
    emit(cid, bytes)?;
    Ok(cid)
}

impl Tx<'_, RW> {
    /// Deletes all state (flat, tries, history) but keeps blocks, so a snapshot can be imported
    /// into a datadir that only holds genesis.
    pub fn reset_state(&self) -> Result<()> {
        for table in [
            t::ACCOUNTS,
            t::STORAGE,
            t::TRIE_ACC,
            t::TRIE_STO,
            t::ACC_HIST,
            t::STO_HIST,
            t::HIST_KEYS,
        ] {
            self.clear_table(table)?;
        }
        Ok(())
    }

    /// Applies one account chunk of a snapshot (entries in order; continuations add storage).
    /// Returns the state root after it.
    pub fn apply_snapshot_accounts(&self, chunk: &[u8]) -> Result<B256> {
        let entries = decode_accounts(chunk).map_err(|_| StoreError::Corrupt("snapshot chunk"))?;
        let mut changes = StateChangeset::default();
        for e in entries {
            if e.a.len() != 20 || e.b.len() != 32 || e.c.len() != 32 {
                return Err(StoreError::Corrupt("snapshot account"));
            }
            let address = Address::from_slice(&e.a);
            if !e.x {
                changes.accounts.push((
                    address,
                    Some(AccountInfo {
                        nonce: e.n,
                        balance: U256::from_be_slice(&e.b),
                        code_hash: B256::from_slice(&e.c),
                        ..Default::default()
                    }),
                ));
            }
            let storage =
                e.s.iter()
                    .map(|(k, v)| {
                        if k.len() == 32 && v.len() == 32 {
                            Ok((U256::from_be_slice(k), U256::from_be_slice(v)))
                        } else {
                            Err(StoreError::Corrupt("snapshot slot"))
                        }
                    })
                    .collect::<Result<Vec<_>>>()?;
            if !storage.is_empty() {
                changes.storage.push(PlainStorageChangeset {
                    address,
                    wipe_storage: false,
                    storage,
                });
            }
        }
        self.apply_changes(&changes)
    }

    /// Stores the codes of one code chunk.
    pub fn apply_snapshot_codes(&self, chunk: &[u8]) -> Result<()> {
        for code in decode_codes(chunk).map_err(|_| StoreError::Corrupt("code chunk"))? {
            let c = Bytecode::new_raw(code.into_vec().into());
            self.put_raw(t::CODES, c.hash_slow().as_slice(), c.original_byte_slice())?;
        }
        Ok(())
    }

    /// Records the checkpoint block after a snapshot import: the block itself (as the head, with
    /// its total difficulty) and its envelope. Older blocks are unknown except for headers added
    /// with [`Tx::put_header_only`]; state history starts here.
    pub fn put_checkpoint_block(
        &self,
        block: &StoredBlock,
        envelope: &Cid,
        td: U256,
    ) -> Result<()> {
        let number = block.header.number;
        self.put_block(block, &[])?;
        self.put_raw(t::TD, &num_key(number), &td.to_be_bytes::<32>())?;
        self.put_raw(t::ENVELOPES, &num_key(number), &envelope.to_bytes())?;
        self.put_raw(t::META, META_BASE, &num_key(number))?;
        Ok(())
    }

    /// Stores an ancestor header without its body (backfill after a checkpoint import).
    pub fn put_header_only(&self, header: &Header, envelope: &Cid) -> Result<()> {
        use alloy_rlp::Encodable;
        let n = num_key(header.number);
        let mut buf = Vec::new();
        header.encode(&mut buf);
        self.put_raw(t::HEADERS, &n, &buf)?;
        self.put_raw(t::HASH_NUM, header.hash_slow().as_slice(), &n)?;
        self.put_raw(t::ENVELOPES, &n, &envelope.to_bytes())
    }

    /// Drops the bodies of block `number` (transactions, senders, receipts, the transaction index
    /// and the body chunks in the blockstore). Its header and envelope stay. Returns whether
    /// anything was removed.
    pub fn prune_block(&self, number: u64) -> Result<bool> {
        let n = num_key(number);
        let Some(block) = self.block(number)? else { return Ok(false) };
        if self.get_raw(t::BODIES, &n)?.is_none() {
            return Ok(false);
        }
        for tx in &block.transactions {
            let h = tx.tx_hash();
            if self.tx_location(h)?.map(|(b, _)| b) == Some(number) {
                self.del_raw(t::TX_INDEX, h.as_slice())?;
            }
        }
        for table in [t::BODIES, t::SENDERS, t::RECEIPTS] {
            self.del_raw(table, &n)?;
        }
        if let Some(root) = self.envelope_root(number)?
            && let Some(bytes) = self.ipld(&root)?
            && let Ok(env) = bolt_ipld::Envelope::decode(&bytes)
        {
            for c in &env.chunks {
                self.del_raw(t::IPLD, &c.to_bytes())?;
            }
        }
        Ok(true)
    }

    /// Records the snapshot roots this node keeps (newest last).
    pub fn set_snapshot_roots(&self, roots: &[Cid]) -> Result<()> {
        let mut v = Vec::new();
        for r in roots {
            let b = r.to_bytes();
            v.push(b.len() as u8);
            v.extend_from_slice(&b);
        }
        self.put_raw(t::META, META_SNAPSHOTS, &v)
    }

    /// Deletes an IPFS block.
    pub fn del_ipld(&self, cid: &Cid) -> Result<()> {
        self.del_raw(t::IPLD, &cid.to_bytes())
    }
}

impl<K: TransactionKind> Tx<'_, K> {
    /// First block whose state history and body this node has (0 unless it started from a
    /// snapshot).
    pub fn base(&self) -> Result<u64> {
        Ok(self
            .get_raw(t::META, META_BASE)?
            .map(|v| u64::from_be_bytes(v.try_into().unwrap_or_default()))
            .unwrap_or(0))
    }

    /// Snapshot roots this node keeps (newest last).
    pub fn snapshot_roots(&self) -> Result<Vec<Cid>> {
        let v = self.get_raw(t::META, META_SNAPSHOTS)?.unwrap_or_default();
        let mut out = Vec::new();
        let mut rest = v.as_slice();
        while let Some((&n, tail)) = rest.split_first() {
            let n = n as usize;
            if tail.len() < n {
                return Err(StoreError::Corrupt("snapshot roots"));
            }
            out.push(Cid::try_from(&tail[..n]).map_err(|_| StoreError::Corrupt("snapshot roots"))?);
            rest = &tail[n..];
        }
        Ok(out)
    }

    /// Whether block `number`'s body is stored (not pruned, not before the base).
    pub fn has_body(&self, number: u64) -> Result<bool> {
        Ok(self.get_raw(t::BODIES, &num_key(number))?.is_some())
    }
}
