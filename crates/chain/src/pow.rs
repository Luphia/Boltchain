//! Mined blocks: fork choice by total difficulty, bounded reorganisations and sealing (ADR 0007
//! §1–§2).
//!
//! The canonical chain lives in the store. A mined block on another branch is kept here, checked
//! as far as its header allows (difficulty, seal), and only executed once its branch has more
//! total difficulty than the canonical head. The switch then undoes canonical blocks through the
//! store's state history and imports the branch. It is refused when the fork point is more than
//! [`MAX_REORG_DEPTH`] blocks deep or below a block produced by the PoS committee (final).

use crate::{BuiltBlock, Chain, ChainError, Result};
use alloy_consensus::{Header, TxEnvelope};
use alloy_primitives::{B64, B256, U256};
use bolt_ipld::Cid;
use bolt_primitives::params::MAX_REORG_DEPTH;

/// How far ahead of the local clock a mined block's timestamp may be.
pub const MAX_FUTURE_SECONDS: u64 = 15;

/// A mined block not on the canonical chain.
#[derive(Debug, Clone)]
pub struct SideBlock {
    /// Header.
    pub header: Header,
    /// Transactions.
    pub transactions: Vec<TxEnvelope>,
    /// Envelope CID it was announced under, if known.
    pub root: Option<Cid>,
    /// Total difficulty up to and including this block.
    pub td: U256,
    /// Certificates its envelope carries (phase B checkpoint QC, audit certificates).
    pub cert: crate::Certs,
}

/// What importing a mined block did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MinedOutcome {
    /// Already known (canonical or side).
    Known,
    /// Extended the canonical chain.
    Extended,
    /// Stored on a side branch with less total difficulty than the head.
    Side,
    /// Its branch became canonical: `depth` blocks were undone.
    Reorged {
        /// Canonical blocks replaced.
        depth: u64,
    },
}

impl Chain {
    /// Where block `hash` is: `Some((header, td, canonical))`.
    fn locate(&self, hash: &B256) -> Result<Option<(Header, U256, bool)>> {
        {
            let r = self.store.reader()?;
            if let Some(n) = r.block_number(hash)? {
                let h = r.header(n)?.ok_or(ChainError::UnknownParent(*hash))?;
                let td = r.total_difficulty(n)?.unwrap_or_default();
                return Ok(Some((h, td, true)));
            }
        }
        Ok(self.side.lock().get(hash).map(|b| (b.header.clone(), b.td, false)))
    }

    /// Hash of the block at `height` on the branch ending at `tip` (canonical or side).
    fn ancestor_hash(&self, tip: &B256, height: u64) -> Result<B256> {
        let mut cur = *tip;
        loop {
            if let Some((h, _, canonical)) = self.locate(&cur)? {
                if h.number == height {
                    return Ok(cur);
                }
                if canonical {
                    return self
                        .store
                        .reader()?
                        .block_hash(height)?
                        .ok_or(ChainError::UnknownParent(cur));
                }
                if h.number < height {
                    return Err(ChainError::UnknownParent(cur));
                }
                cur = h.parent_hash;
            } else {
                return Err(ChainError::UnknownParent(cur));
            }
        }
    }

    /// Header checks a mined block must pass before it is kept anywhere: parent link, time,
    /// difficulty, seal, and not past the switch to PoS.
    fn check_mined_header(&self, header: &Header, parent: &Header) -> Result<()> {
        let invalid = ChainError::InvalidBlock;
        if header.number != parent.number + 1 {
            return Err(invalid("wrong number".into()));
        }
        if header.timestamp <= parent.timestamp {
            return Err(invalid("timestamp not after parent".into()));
        }
        if header.difficulty.is_zero() {
            return Err(invalid("not a mined block".into()));
        }
        if self.phase()?.is_pos(&self.rules, header.number) {
            return Err(invalid(format!(
                "mined block {} is past the switch to PoS",
                header.number
            )));
        }
        let expected = self.difficulty_after(parent)?;
        if header.difficulty != expected {
            return Err(invalid(format!("difficulty {} != {expected} (ASERT)", header.difficulty)));
        }
        if header.extra_data.len() > 32 {
            return Err(invalid("extra data over 32 bytes".into()));
        }
        let seed = self.ancestor_hash(&header.parent_hash, bolt_pow::seed_height(header.number))?;
        self.check_seal(header, &seed)
    }

    /// Imports a mined block from the network or from this node's miner. Its parent must be
    /// known. The block extends the canonical chain, waits on a side branch, or triggers a
    /// reorganisation when its branch has more total difficulty (ties keep the current head).
    pub fn import_mined(
        &self,
        header: &Header,
        transactions: Vec<TxEnvelope>,
        root: Option<Cid>,
    ) -> Result<MinedOutcome> {
        self.import_mined_with_cert(header, transactions, root, crate::Certs::default())
    }

    /// [`Chain::import_mined`] for a block whose envelope carries a checkpoint QC (phase B).
    pub fn import_mined_with_cert(
        &self,
        header: &Header,
        transactions: Vec<TxEnvelope>,
        root: Option<Cid>,
        cert: impl Into<crate::Certs>,
    ) -> Result<MinedOutcome> {
        let cert = cert.into();
        let _guard = self.import_lock.lock();
        let hash = header.hash_slow();
        if self.locate(&hash)?.is_some() {
            return Ok(MinedOutcome::Known);
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if header.timestamp > now + MAX_FUTURE_SECONDS {
            return Err(ChainError::InvalidBlock(format!(
                "timestamp {} is in the future",
                header.timestamp
            )));
        }
        let (parent, parent_td, parent_canonical) = self
            .locate(&header.parent_hash)?
            .ok_or(ChainError::UnknownParent(header.parent_hash))?;
        if let Some((fin, _)) = self.finalized()?
            && parent_canonical
            && parent.number < fin
        {
            return Err(ChainError::DeepReorg(format!(
                "block {} forks below the finalized checkpoint {fin}",
                header.number
            )));
        }
        let head = self.head()?;
        if header.parent_hash == head.hash_slow() {
            // Full verification (seal first, then execution).
            if header.difficulty.is_zero() {
                return Err(ChainError::InvalidBlock("not a mined block".into()));
            }
            self.import_final(header, transactions, root, cert)?;
            return Ok(MinedOutcome::Extended);
        }
        self.check_mined_header(header, &parent)?;
        let td = parent_td + header.difficulty;
        self.side
            .lock()
            .insert(hash, SideBlock { header: header.clone(), transactions, root, td, cert });
        // More work wins; equal work goes to the lower hash, so every node picks the same tip.
        let head_td = self.head_total_difficulty()?;
        if td < head_td || (td == head_td && hash >= head.hash_slow()) {
            return Ok(MinedOutcome::Side);
        }
        match self.reorg_to(&hash) {
            Ok(depth) => Ok(MinedOutcome::Reorged { depth }),
            Err(e) => Err(e),
        }
    }

    /// Makes the side branch ending at `tip` canonical.
    fn reorg_to(&self, tip: &B256) -> Result<u64> {
        // The branch, newest first, down to its fork point on the canonical chain.
        let mut branch: Vec<SideBlock> = Vec::new();
        let mut cur = *tip;
        let fork = loop {
            let side = self.side.lock().get(&cur).cloned();
            match side {
                Some(b) => {
                    cur = b.header.parent_hash;
                    branch.push(b);
                }
                None => {
                    let r = self.store.reader()?;
                    break r.block_number(&cur)?.ok_or(ChainError::UnknownParent(cur))?;
                }
            }
        };
        branch.reverse();
        let head = self.head()?;
        let depth = head.number - fork;
        if let Some((fin, _)) = self.finalized()?
            && fork < fin
        {
            return Err(ChainError::DeepReorg(format!(
                "fork point {fork} is below the finalized checkpoint {fin}"
            )));
        }
        if depth > MAX_REORG_DEPTH {
            return Err(ChainError::DeepReorg(format!(
                "fork point {fork} is {depth} blocks below head {}",
                head.number
            )));
        }
        {
            let r = self.store.reader()?;
            for n in fork + 1..=head.number {
                if r.header(n)?.is_some_and(|h| h.difficulty.is_zero()) {
                    return Err(ChainError::DeepReorg(format!(
                        "block {n} was produced by the PoS committee"
                    )));
                }
            }
        }
        tracing::warn!(depth, fork, new_tip = %tip, "reorganising to a heavier mined branch");
        let undone = self.unwind_to(fork)?;
        for (i, b) in branch.iter().enumerate() {
            let res = self.import_final(&b.header, b.transactions.clone(), b.root, b.cert.clone());
            if let Err(e) = res {
                tracing::warn!(
                    number = b.header.number,
                    "heavier branch has an invalid block: {e}"
                );
                // Forget the invalid block and its descendants; restore the old branch.
                {
                    let mut side = self.side.lock();
                    for d in &branch[i..] {
                        side.remove(&d.header.hash_slow());
                    }
                }
                self.unwind_to(fork)?;
                for o in &undone {
                    self.side.lock().remove(&o.header.hash_slow());
                    self.import_final(&o.header, o.transactions.clone(), o.root, o.cert.clone())?;
                }
                return Err(e);
            }
            self.side.lock().remove(&b.header.hash_slow());
        }
        // The replaced blocks stay available as a side branch.
        let mut side = self.side.lock();
        for o in undone {
            side.insert(o.header.hash_slow(), o);
        }
        Ok(depth)
    }

    /// Undoes canonical blocks down to `number`. Returns them oldest first, as side blocks.
    fn unwind_to(&self, number: u64) -> Result<Vec<SideBlock>> {
        self.pending.lock().clear();
        let mut undone = Vec::new();
        let w = self.store.writer()?;
        loop {
            let head = w.head()?.unwrap_or(0);
            if head <= number {
                break;
            }
            let root = w.envelope_root(head)?;
            let td = w.total_difficulty(head)?.unwrap_or_default();
            let cert = match root {
                Some(r) => w
                    .ipld(&r)?
                    .and_then(|b| bolt_ipld::Envelope::decode(&b).ok())
                    .map(|e| crate::Certs::of(&e))
                    .unwrap_or_default(),
                None => crate::Certs::default(),
            };
            let (block, state_root) = w.unwind_head()?;
            let parent_root = w.header(head - 1)?.map(|h| h.state_root);
            if parent_root != Some(state_root) {
                return Err(ChainError::Exec(format!(
                    "state root after undoing block {head} does not match its parent"
                )));
            }
            undone.push(SideBlock {
                header: block.header,
                transactions: block.transactions,
                root,
                td,
                cert,
            });
        }
        w.commit()?;
        undone.reverse();
        Ok(undone)
    }

    /// Latest block final by stake (phase B checkpoint): (number, hash).
    pub fn finalized(&self) -> Result<Option<(u64, B256)>> {
        Ok(self.store.reader()?.finalized()?)
    }

    /// Records block `hash` at `height` as final (a certified checkpoint, ADR 0007 §4). If it is
    /// on a side branch, that branch becomes canonical whatever its total difficulty: stake
    /// finality overrides work. Fork choice never goes below it again. Returns whether anything
    /// changed.
    pub fn finalize(&self, hash: &B256, height: u64) -> Result<bool> {
        let _guard = self.import_lock.lock();
        if self.finalized()?.is_some_and(|(n, _)| n >= height) {
            return Ok(false);
        }
        let (header, _, canonical) = self.locate(hash)?.ok_or(ChainError::UnknownParent(*hash))?;
        if header.number != height {
            return Err(ChainError::InvalidBlock(format!(
                "checkpoint {hash} is block {}, not {height}",
                header.number
            )));
        }
        if !canonical {
            tracing::warn!(height, %hash, "finalized checkpoint is on another branch; switching");
            self.reorg_to(hash)?;
        }
        let w = self.store.writer()?;
        w.set_finalized(height, hash)?;
        w.commit()?;
        let horizon = height;
        self.side.lock().retain(|_, b| b.header.number > horizon);
        tracing::debug!(height, %hash, "checkpoint final");
        Ok(true)
    }

    /// Builds (without committing) the next mined block on the head for a miner to seal. In
    /// phase B it may carry the latest checkpoint QC (`cert`), which pays its signers.
    pub fn build_template(
        &self,
        candidates: impl IntoIterator<Item = (TxEnvelope, alloy_primitives::Address)>,
        timestamp: u64,
        beneficiary: alloy_primitives::Address,
        extra_data: alloy_primitives::Bytes,
        cert: impl Into<crate::Certs>,
    ) -> Result<BuiltBlock> {
        let head = self.head()?.hash_slow();
        let built = self.build_on(&head, candidates, timestamp, beneficiary, extra_data, cert)?;
        if built.header.difficulty.is_zero() {
            self.pending.lock().remove(&built.hash);
            return Err(ChainError::InvalidBlock("PoS has started: nothing to mine".into()));
        }
        Ok(built)
    }

    /// The RandomBOLT key for a mined block after `parent`.
    pub fn seal_key(&self, parent: &B256, number: u64) -> Result<B256> {
        Ok(bolt_pow::key_for(&self.ancestor_hash(parent, bolt_pow::seed_height(number))?))
    }

    /// Seals a template with `nonce` and imports it. Returns the sealed header.
    pub fn seal_and_import(
        &self,
        template: &BuiltBlock,
        nonce: u64,
    ) -> Result<(Header, MinedOutcome)> {
        let p = self.pending(&template.hash);
        self.pending.lock().remove(&template.hash);
        let p = p.ok_or(ChainError::UnknownParent(template.hash))?;
        let txs = p.executed.transactions.clone();
        let cert = crate::Certs::of(&p.bundle.envelope);
        let mut header = template.header.clone();
        header.nonce = B64::from(nonce);
        let outcome = self.import_mined_with_cert(&header, txs, None, cert)?;
        Ok((header, outcome))
    }

    /// Whether a mined block is known (canonical or side).
    pub fn knows(&self, hash: &B256) -> Result<bool> {
        Ok(self.locate(hash)?.is_some())
    }
}
