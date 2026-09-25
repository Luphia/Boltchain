//! Storage layer on the chain side (ADR 0009): state snapshots at epoch ends, checkpoint import
//! (a new node starts from a snapshot instead of genesis), pruning of old block bodies.

use crate::{Chain, ChainError, Result};
use alloy_consensus::{Header, transaction::SignerRecoverable};
use alloy_primitives::{Address, B256, U256};
use bolt_ipld::{
    Cid, Envelope,
    history::{SnapshotRoot, decode_accounts},
};
use bolt_primitives::params::MAX_REORG_DEPTH;
use bolt_store::StoredBlock;
use std::collections::HashSet;

/// Snapshots kept by each node (older ones are deleted once a new one is complete).
pub const KEEP_SNAPSHOTS: usize = 2;

/// Blocks before a checkpoint whose headers are fetched with it (the EVM's `BLOCKHASH` window).
pub const BACKFILL_HEADERS: u64 = 256;

fn corrupt(what: &str) -> ChainError {
    ChainError::InvalidBlock(format!("snapshot: {what}"))
}

impl Chain {
    /// Whether block `number` can no longer be reverted: produced under PoS (only final blocks
    /// are stored), covered by a phase-B checkpoint, or buried deeper than any allowed
    /// reorganisation.
    pub fn is_final(&self, number: u64) -> Result<bool> {
        if self.phase()?.is_pos(&self.rules, number) {
            return Ok(true);
        }
        if self.finalized()?.is_some_and(|(n, _)| n >= number) {
            return Ok(true);
        }
        Ok(self.head()?.number >= number + MAX_REORG_DEPTH)
    }

    /// Snapshot roots this node keeps, oldest first, with their block numbers.
    pub fn snapshots(&self) -> Result<Vec<(u64, Cid)>> {
        let r = self.store.reader()?;
        let mut out = Vec::new();
        for c in r.snapshot_roots()? {
            if let Some(b) = r.ipld(&c)?
                && let Ok(s) = SnapshotRoot::decode(&b)
            {
                out.push((s.number, c));
            }
        }
        Ok(out)
    }

    /// Takes a snapshot of the head if the head is the last block of an epoch and has none yet.
    /// Its blocks go to the blockstore (served over Bitswap and the gateway); snapshots beyond
    /// [`KEEP_SNAPSHOTS`] are deleted. Returns the block number and root.
    pub fn take_snapshot(&self) -> Result<Option<(u64, Cid)>> {
        let r = self.store.reader()?;
        let Some(number) = r.head()? else { return Ok(None) };
        if number == 0 || self.rules.epoch_end(self.rules.epoch_of(number)) != number {
            return Ok(None);
        }
        let existing = self.snapshots()?;
        if existing.iter().any(|(n, _)| *n == number) {
            return Ok(None);
        }
        let header = r.header(number)?.ok_or(missing(number))?;
        let envelope = r.envelope_root(number)?.ok_or(missing(number))?;
        let td = r.total_difficulty(number)?.unwrap_or_default();
        let started = std::time::Instant::now();
        // Build from this read snapshot on one thread while another writes the blocks.
        let (tx, rx) = std::sync::mpsc::sync_channel::<Vec<(Cid, Vec<u8>)>>(4);
        let store = &self.store;
        let (root, blocks) = std::thread::scope(|s| -> Result<(Cid, usize)> {
            let writer = s.spawn(move || -> Result<usize> {
                let mut n = 0;
                for batch in rx {
                    let w = store.writer()?;
                    for (c, b) in &batch {
                        w.put_ipld(c, b)?;
                    }
                    w.commit()?;
                    n += batch.len();
                }
                Ok(n)
            });
            let mut batch = Vec::new();
            let mut size = 0usize;
            let root = bolt_store::build_snapshot(
                &r,
                self.config.chain_id,
                &header,
                envelope,
                td,
                |c, b| {
                    size += b.len();
                    batch.push((c, b));
                    if size > 32 << 20 {
                        size = 0;
                        tx.send(std::mem::take(&mut batch))
                            .map_err(|_| bolt_store::StoreError::Corrupt("snapshot writer"))?;
                    }
                    Ok(())
                },
            );
            if !batch.is_empty() {
                let _ = tx.send(batch);
            }
            drop(tx);
            let n =
                writer.join().map_err(|_| ChainError::Exec("snapshot writer panicked".into()))?;
            Ok((root?, n?))
        })?;
        drop(r);
        // Record it and drop the oldest ones (keeping blocks the newer snapshots share).
        let mut roots: Vec<Cid> = existing.iter().map(|(_, c)| *c).collect();
        roots.push(root);
        let w = self.store.writer()?;
        if roots.len() > KEEP_SNAPSHOTS {
            let old: Vec<Cid> = roots.drain(..roots.len() - KEEP_SNAPSHOTS).collect();
            let mut keep = HashSet::new();
            for c in &roots {
                if let Some(s) = w.ipld(c)?.and_then(|b| SnapshotRoot::decode(&b).ok()) {
                    keep.extend(s.accounts.into_iter().chain(s.codes));
                }
            }
            for c in old {
                if let Some(s) = w.ipld(&c)?.and_then(|b| SnapshotRoot::decode(&b).ok()) {
                    for chunk in s.accounts.iter().chain(&s.codes) {
                        if !keep.contains(chunk) {
                            w.del_ipld(chunk)?;
                        }
                    }
                }
                w.del_ipld(&c)?;
            }
        }
        w.set_snapshot_roots(&roots)?;
        w.commit()?;
        tracing::info!(
            number,
            %root,
            blocks,
            ms = started.elapsed().as_millis() as u64,
            "state snapshot taken"
        );
        Ok(Some((number, root)))
    }

    /// Every CID a node must hold to start from snapshot `root` (whose block is `trusted`):
    /// fetched in stages by the caller (root, then envelope and chunks, then ancestors) and
    /// written to the blockstore before [`Chain::import_checkpoint`].
    pub fn snapshot_parts(root: &SnapshotRoot) -> Vec<Cid> {
        std::iter::once(root.envelope)
            .chain(root.accounts.iter().copied())
            .chain(root.codes.iter().copied())
            .collect()
    }

    /// Heights before checkpoint block `number` whose headers and envelopes must be present:
    /// the `BLOCKHASH` window and the rest of the checkpoint's epoch (the next block records the
    /// epoch's index).
    pub fn backfill_from(&self, number: u64) -> u64 {
        let epoch_start = self.rules.epoch_of(number) * self.rules.epoch_slots + 1;
        epoch_start.min(number.saturating_sub(BACKFILL_HEADERS)).max(1)
    }

    /// Starts this (fresh) node from snapshot `root`, whose blocks, the checkpoint block's
    /// envelope, header and body, and the envelopes and headers back to
    /// [`Chain::backfill_from`] are already in the blockstore. The block must hash to `trusted`
    /// (weak subjectivity: the operator's checkpoint), and the rebuilt state must match its
    /// state root. The block becomes the head, final, and the base of local history.
    pub fn import_checkpoint(&self, root: &Cid, trusted: &B256) -> Result<Header> {
        let _guard = self.import_lock.lock();
        let r = self.store.reader()?;
        if r.head()?.unwrap_or(0) != 0 {
            return Err(ChainError::Exec("checkpoint import needs a fresh datadir".into()));
        }
        let get = |c: &Cid| r.ipld(c).ok().flatten();
        let snap = SnapshotRoot::decode(&get(root).ok_or(corrupt("root missing"))?)
            .map_err(|_| corrupt("root"))?;
        if snap.chain_id != self.config.chain_id || snap.hash.as_slice() != trusted.as_slice() {
            return Err(corrupt("not the trusted block"));
        }
        let env = Envelope::decode(&get(&snap.envelope).ok_or(corrupt("envelope missing"))?)
            .map_err(|_| corrupt("envelope"))?;
        let block = bolt_ipld::decode_block(env, get).map_err(|e| corrupt(&e.to_string()))?;
        let header = block.header.clone();
        if header.hash_slow() != *trusted || header.number != snap.number {
            return Err(corrupt("header does not match the trusted hash"));
        }
        let senders: Vec<Address> = block
            .transactions
            .iter()
            .map(|t| t.recover_signer())
            .collect::<Result<_, _>>()
            .map_err(|_| corrupt("bad signature"))?;
        // Ancestors, newest first, each checked against its child's parent hash.
        let lower = self.backfill_from(header.number);
        let mut ancestors: Vec<(Header, Cid)> = Vec::new();
        let (mut parent_env, mut parent_hash) = (block.envelope.parent, header.parent_hash);
        while let Some(pc) = parent_env {
            let e = Envelope::decode(&get(&pc).ok_or(corrupt("ancestor envelope missing"))?)
                .map_err(|_| corrupt("ancestor envelope"))?;
            if e.height < lower || e.height == 0 {
                break;
            }
            let hb = get(&e.header).ok_or(corrupt("ancestor header missing"))?;
            let h = <Header as alloy_rlp::Decodable>::decode(&mut hb.as_slice())
                .map_err(|_| corrupt("ancestor header"))?;
            if h.hash_slow() != parent_hash {
                return Err(corrupt("ancestor chain broken"));
            }
            (parent_env, parent_hash) = (e.parent, h.parent_hash);
            ancestors.push((h, pc));
        }
        let mut accounts = Vec::with_capacity(snap.accounts.len());
        for c in &snap.accounts {
            let b = get(c).ok_or(corrupt("chunk missing"))?;
            bolt_ipld::verify(c, &b).map_err(|_| corrupt("chunk hash"))?;
            decode_accounts(&b).map_err(|_| corrupt("chunk"))?;
            accounts.push(b);
        }
        let mut codes = Vec::with_capacity(snap.codes.len());
        for c in &snap.codes {
            let b = get(c).ok_or(corrupt("code chunk missing"))?;
            bolt_ipld::verify(c, &b).map_err(|_| corrupt("code chunk hash"))?;
            codes.push(b);
        }
        drop(r);

        let w = self.store.writer()?;
        w.reset_state()?;
        let mut state_root = alloy_trie::EMPTY_ROOT_HASH;
        for b in &accounts {
            state_root = w.apply_snapshot_accounts(b)?;
        }
        for b in &codes {
            w.apply_snapshot_codes(b)?;
        }
        if state_root != header.state_root {
            return Err(corrupt(&format!(
                "rebuilt state root {state_root} != header {}",
                header.state_root
            )));
        }
        let td = U256::from_be_slice(&snap.td);
        let stored =
            StoredBlock { header: header.clone(), transactions: block.transactions, senders };
        w.put_checkpoint_block(&stored, &snap.envelope, td)?;
        for (h, env) in &ancestors {
            w.put_header_only(h, env)?;
        }
        w.set_finalized(header.number, trusted)?;
        w.commit()?;
        tracing::info!(
            number = header.number,
            hash = %trusted,
            accounts = accounts.len(),
            ancestors = ancestors.len(),
            "started from a state snapshot"
        );
        Ok(header)
    }

    /// Drops the bodies of every block of `epoch` (headers and envelopes stay). Returns how many
    /// blocks were pruned.
    pub fn prune_epoch(&self, epoch: u64) -> Result<u64> {
        let first = epoch * self.rules.epoch_slots + 1;
        let w = self.store.writer()?;
        let mut n = 0;
        for h in first..=self.rules.epoch_end(epoch) {
            if w.prune_block(h)? {
                n += 1;
            }
        }
        w.commit()?;
        Ok(n)
    }

    /// The IPFS blocks of `epoch`'s history this node lacks: body chunks of its blocks (their
    /// envelopes are always kept). Empty once the epoch is complete locally.
    pub fn missing_epoch_data(&self, epoch: u64) -> Result<Vec<Cid>> {
        let first = epoch * self.rules.epoch_slots + 1;
        let r = self.store.reader()?;
        let mut out = Vec::new();
        for h in first..=self.rules.epoch_end(epoch) {
            let Some(root) = r.envelope_root(h)? else { continue };
            let Some(env) = r.ipld(&root)?.and_then(|b| Envelope::decode(&b).ok()) else {
                out.push(root);
                continue;
            };
            for c in env.chunks {
                if r.ipld(&c)?.is_none() {
                    out.push(c);
                }
            }
        }
        Ok(out)
    }
}

fn missing(number: u64) -> ChainError {
    ChainError::Exec(format!("block {number} missing"))
}
