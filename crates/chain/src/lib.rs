//! Chain state machine: genesis initialisation, block production and block import.
//!
//! Execution reads a consistent snapshot of the store while RPC readers keep running; the only
//! write transaction is the final commit of each block (state, tries, block, history pruning).

use alloy_consensus::{Header, Transaction as _, TxEnvelope, transaction::SignerRecoverable};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
mod overlay;

use bolt_exec::{
    BlockExecutor, BlockInput, BlockParams, ExecutedBlock, TxRejection, next_base_fee,
};
use bolt_ipld::{BlockBundle, Cid};
use bolt_primitives::{Genesis, genesis::ChainConfig};
use bolt_store::{InitAccount, StateView, Store, StoreError, StoredBlock};
use bolt_system::{CertVotes, EpochRules};
use parking_lot::Mutex;
use revm::database::{OriginalValuesKnown, states::StateChangeset};
use std::{collections::BTreeMap, path::Path};
use std::{collections::HashMap, sync::Arc};

/// Chain error.
#[derive(Debug, thiserror::Error)]
pub enum ChainError {
    /// Storage failure.
    #[error("store: {0}")]
    Store(#[from] StoreError),
    /// The datadir belongs to a different chain.
    #[error("datadir genesis {stored} does not match genesis file {expected}")]
    GenesisMismatch {
        /// Genesis hash in the datadir.
        stored: B256,
        /// Genesis hash of the file.
        expected: B256,
    },
    /// Fatal execution error.
    #[error("execution: {0}")]
    Exec(String),
    /// A block failed validation.
    #[error("invalid block: {0}")]
    InvalidBlock(String),
    /// The parent is neither the committed head nor a known pending block.
    #[error("unknown parent {0}")]
    UnknownParent(B256),
}

/// Result alias.
pub type Result<T, E = ChainError> = std::result::Result<T, E>;

/// Outcome of producing a block.
#[derive(Debug, Clone)]
pub struct BuiltBlock {
    /// The sealed header.
    pub header: Header,
    /// Its hash.
    pub hash: B256,
    /// Hashes of included transactions.
    pub included: Vec<B256>,
    /// Transactions that were tried and rejected: hash, reason, and whether the rejection is
    /// permanent (invalid) rather than "did not fit in this block".
    pub rejected: Vec<(B256, String, bool)>,
    /// The block's IPFS representation (already stored in the blockstore).
    pub bundle: BlockBundle,
}

/// The chain.
#[derive(Debug)]
pub struct Chain {
    store: Store,
    config: ChainConfig,
    rules: EpochRules,
    genesis_hash: B256,
    pending: Mutex<HashMap<B256, Arc<PendingBlock>>>,
}

/// A pending block, the included transaction hashes and the skipped ones (hash, reason, drop).
type Executed = (Arc<PendingBlock>, Vec<B256>, Vec<(B256, String, bool)>);

impl Chain {
    /// Opens the datadir, writing genesis on first start.
    pub fn open(datadir: impl AsRef<Path>, genesis: &Genesis) -> Result<Self> {
        let store = Store::open(datadir)?;
        let genesis_header =
            bolt_system::genesis_header(genesis).map_err(|e| ChainError::Exec(e.to_string()))?;
        let expected = genesis_header.hash_slow();
        let head = store.reader()?.head()?;
        match head {
            None => {
                let alloc: BTreeMap<Address, InitAccount> = bolt_system::genesis_alloc(genesis)
                    .map_err(|e| ChainError::Exec(e.to_string()))?
                    .into_iter()
                    .map(|(a, acc)| {
                        let storage = acc
                            .storage
                            .into_iter()
                            .filter(|(_, v)| !v.is_zero())
                            .map(|(k, v)| (k, U256::from_be_bytes(v.0)))
                            .collect();
                        (
                            a,
                            InitAccount {
                                nonce: acc.nonce,
                                balance: acc.balance,
                                code: acc.code,
                                storage,
                            },
                        )
                    })
                    .collect();
                let w = store.writer()?;
                let root = w.init_state(&alloc)?;
                let header = genesis_header;
                if root != header.state_root {
                    return Err(ChainError::Exec(format!(
                        "genesis state root mismatch: store {root}, genesis {}",
                        header.state_root
                    )));
                }
                let bundle = bolt_ipld::bundle(&header, &[], None, vec![]);
                w.put_ipld_bundle(0, &bundle.root, &bundle.blocks)?;
                w.put_block(&StoredBlock { header, transactions: vec![], senders: vec![] }, &[])?;
                w.commit()?;
                tracing::info!(hash = %expected, "initialised genesis");
            }
            Some(_) => {
                let stored = store.reader()?.block_hash(0)?.unwrap_or_default();
                if stored != expected {
                    return Err(ChainError::GenesisMismatch { stored, expected });
                }
            }
        }
        Ok(Self {
            store,
            config: genesis.config.clone(),
            rules: EpochRules::from_config(&genesis.config),
            genesis_hash: expected,
            pending: Mutex::new(HashMap::new()),
        })
    }

    /// The underlying store (for RPC reads).
    pub fn store(&self) -> &Store {
        &self.store
    }

    /// Chain parameters.
    pub fn config(&self) -> &ChainConfig {
        &self.config
    }

    /// Genesis block hash.
    pub fn genesis_hash(&self) -> B256 {
        self.genesis_hash
    }

    /// Current head header.
    pub fn head(&self) -> Result<Header> {
        let r = self.store.reader()?;
        let n = r.head()?.unwrap_or(0);
        r.header(n)?.ok_or_else(|| ChainError::Exec("missing head header".into()))
    }

    /// Epoch rules.
    pub fn rules(&self) -> &EpochRules {
        &self.rules
    }

    /// Base fee the next block will use.
    pub fn next_base_fee(&self) -> Result<u64> {
        let r = self.store.reader()?;
        let p = self.chain_params(&StateView::latest(&r))?;
        Ok(next_base_fee(&self.head()?, p.min_base_fee))
    }

    /// Governance parameters (`ParamRegistry`) in the state `db` (the parent of the next block).
    pub fn chain_params<D: revm::DatabaseRef>(&self, db: D) -> Result<ChainParams>
    where
        D::Error: std::fmt::Debug,
    {
        let p = bolt_system::queries::call(
            db,
            self.config.chain_id,
            bolt_system::addresses::PARAMS,
            bolt_system::abi::IParamRegistry::paramsCall {},
        )
        .map_err(|e| ChainError::Exec(e.to_string()))?;
        Ok(ChainParams { gas_limit: p.gasLimit, min_base_fee: p.minBaseFee })
    }

    fn params_for(
        &self,
        parent: &Header,
        cp: &ChainParams,
        timestamp: u64,
        beneficiary: Address,
        extra_data: Bytes,
    ) -> BlockParams {
        BlockParams {
            input: BlockInput {
                chain_id: self.config.chain_id,
                number: parent.number + 1,
                timestamp,
                beneficiary,
                gas_limit: cp.gas_limit,
                base_fee: next_base_fee(parent, cp.min_base_fee),
                prevrandao: mix_hash(parent, &extra_data),
            },
            parent_hash: parent.hash_slow(),
            parent_beacon_root: B256::ZERO,
            extra_data,
        }
    }

    /// Header of `hash`, whether committed or pending.
    pub fn header_of(&self, hash: &B256) -> Result<Option<Header>> {
        if let Some(p) = self.pending.lock().get(hash) {
            return Ok(Some(p.header.clone()));
        }
        let r = self.store.reader()?;
        match r.block_number(hash)? {
            Some(n) => Ok(r.header(n)?),
            None => Ok(None),
        }
    }

    /// A pending (executed, not yet final) block.
    pub fn pending(&self, hash: &B256) -> Option<Arc<PendingBlock>> {
        self.pending.lock().get(hash).cloned()
    }

    /// Pending blocks from the committed head up to `parent` (oldest first). Empty when `parent`
    /// is the head itself.
    fn pending_chain(&self, head: &Header, parent: &B256) -> Result<Vec<Arc<PendingBlock>>> {
        let head_hash = head.hash_slow();
        let pending = self.pending.lock();
        let mut chain = Vec::new();
        let mut cur = *parent;
        while cur != head_hash {
            let p = pending.get(&cur).ok_or_else(|| ChainError::UnknownParent(cur))?.clone();
            if p.header.number <= head.number {
                return Err(ChainError::UnknownParent(cur));
            }
            cur = p.header.parent_hash;
            chain.push(p);
        }
        chain.reverse();
        Ok(chain)
    }

    /// Executes a block on `parent` (the committed head or a pending block) without committing.
    /// With `expected`, the transactions must all execute and the header must match exactly;
    /// otherwise candidates that do not fit are skipped.
    fn execute_on(
        &self,
        parent_hash: &B256,
        mut params_fn: impl FnMut(&Header, &ChainParams) -> BlockParams,
        txs: Vec<(TxEnvelope, Address)>,
        expected: Option<&Header>,
        qc: Vec<u8>,
    ) -> Result<Executed> {
        let head = self.head()?;
        let ancestors = self.pending_chain(&head, parent_hash)?;
        let parent_header = match ancestors.last() {
            Some(p) => p.header.clone(),
            None => head.clone(),
        };
        let mut changes = overlay::Changes::default();
        for a in &ancestors {
            changes.push(a.header.number, a.hash, &a.changes);
        }
        let (cert_epoch, bitmap) = bolt_consensus::cert_votes(&qc);
        let votes = CertVotes { epoch: cert_epoch, bitmap };

        let mut included = Vec::new();
        let mut rejected = Vec::new();
        let executed = {
            let reader = self.store.reader()?;
            let view = StateView::latest(&reader);
            let db = overlay::Overlay { base: &view, changes: &changes };
            let cp = self.chain_params(&db)?;
            let params = params_fn(&parent_header, &cp);
            let mut ex =
                BlockExecutor::new(&db, params).map_err(|e| ChainError::Exec(e.to_string()))?;
            bolt_system::pre_block(&mut ex, &self.rules, &parent_header, &votes)
                .map_err(|e| ChainError::Exec(format!("system calls: {e}")))?;
            for (tx, sender) in txs {
                if expected.is_none() && ex.gas_remaining() < 21_000 {
                    break;
                }
                let hash = *tx.tx_hash();
                match ex.add(tx, sender).map_err(|e| ChainError::Exec(e.to_string()))? {
                    Ok(_) => included.push(hash),
                    Err(e) if expected.is_some() => {
                        return Err(ChainError::InvalidBlock(format!("tx {hash}: {e}")));
                    }
                    Err(e) => {
                        let permanent = matches!(e, TxRejection::Invalid(_) | TxRejection::Blob);
                        rejected.push((hash, e.to_string(), permanent));
                    }
                }
            }
            ex.finish()
        };
        let number = executed.params.input.number;

        // State root: apply ancestors and this block in a write transaction that is then dropped
        // (aborted). Nothing reaches the database until the block is committed.
        let root = {
            let w = self.store.writer()?;
            for a in &ancestors {
                w.apply_bundle(a.header.number, a.executed.bundle.clone())?;
            }
            w.apply_bundle(number, executed.bundle.clone())?
        };
        let header = executed.header(root);
        if let Some(exp) = expected
            && exp != &header
        {
            return Err(ChainError::InvalidBlock(format!(
                "header mismatch (state root {} vs {}, gas limit {} vs {}, base fee {:?} vs {:?}, mix {} vs {})",
                header.state_root,
                exp.state_root,
                header.gas_limit,
                exp.gas_limit,
                header.base_fee_per_gas,
                exp.base_fee_per_gas,
                header.mix_hash,
                exp.mix_hash
            )));
        }
        let parent_root = match ancestors.last() {
            Some(p) => Some(p.bundle.root),
            None => self.store.reader()?.envelope_root(head.number)?,
        };
        let bundle = bolt_ipld::bundle(&header, &executed.transactions, parent_root, qc);
        // Publish the IPFS blocks right away so peers can fetch them before the block is final
        // (validators must hold the data to vote).
        {
            let w = self.store.writer()?;
            for (cid, data) in &bundle.blocks {
                w.put_ipld(cid, data)?;
            }
            w.commit()?;
        }
        let (changes_plain, _) =
            executed.bundle.to_plain_state_and_reverts(OriginalValuesKnown::Yes);
        let hash = header.hash_slow();
        let pending =
            Arc::new(PendingBlock { hash, header, executed, changes: changes_plain, bundle });
        self.pending.lock().insert(hash, pending.clone());
        Ok((pending, included, rejected))
    }

    /// Builds a block on `parent` from candidate transactions, without committing it.
    #[allow(clippy::too_many_arguments)]
    pub fn build_on(
        &self,
        parent: &B256,
        candidates: impl IntoIterator<Item = (TxEnvelope, Address)>,
        timestamp: u64,
        beneficiary: Address,
        extra_data: Bytes,
        qc: Vec<u8>,
    ) -> Result<BuiltBlock> {
        let beacon = if qc.is_empty() { B256::ZERO } else { keccak256(&qc) };
        let (pending, included, rejected) = self.execute_on(
            parent,
            |ph, cp| {
                let mut p = self.params_for(
                    ph,
                    cp,
                    timestamp.max(ph.timestamp + 1),
                    beneficiary,
                    extra_data.clone(),
                );
                p.parent_beacon_root = beacon;
                p
            },
            candidates.into_iter().collect(),
            None,
            qc.clone(),
        )?;
        Ok(BuiltBlock {
            header: pending.header.clone(),
            hash: pending.hash,
            included,
            rejected,
            bundle: pending.bundle.clone(),
        })
    }

    /// Verifies a block proposed by someone else on top of a committed or pending parent, without
    /// committing it. `qc` must be the envelope's certificate for the parent.
    pub fn verify_block(
        &self,
        header: &Header,
        transactions: Vec<TxEnvelope>,
        qc: Vec<u8>,
    ) -> Result<Arc<PendingBlock>> {
        if let Some(p) = self.pending(&header.hash_slow()) {
            return Ok(p);
        }
        let parent = self
            .header_of(&header.parent_hash)?
            .ok_or(ChainError::UnknownParent(header.parent_hash))?;
        let invalid = ChainError::InvalidBlock;
        if header.number != parent.number + 1 {
            return Err(invalid("wrong number".into()));
        }
        if header.timestamp <= parent.timestamp {
            return Err(invalid("timestamp not after parent".into()));
        }
        // Gas limit, base fee (ParamRegistry) and the RANDAO mix are recomputed during execution
        // and compared with the header.
        let expected_beacon = if qc.is_empty() { B256::ZERO } else { keccak256(&qc) };
        if header.parent_beacon_block_root != Some(expected_beacon) {
            return Err(invalid("parent certificate hash mismatch".into()));
        }
        let senders: Vec<Address> = {
            use rayon::prelude::*;
            transactions
                .par_iter()
                .map(|tx| tx.recover_signer())
                .collect::<Result<_, _>>()
                .map_err(|_| invalid("bad signature".into()))?
        };
        let (h, number) = (header.clone(), header.number);
        let (pending, _, _) = self.execute_on(
            &header.parent_hash,
            |ph, cp| {
                let mut p =
                    self.params_for(ph, cp, h.timestamp, h.beneficiary, h.extra_data.clone());
                p.parent_beacon_root = expected_beacon;
                p
            },
            transactions.into_iter().zip(senders).collect(),
            Some(header),
            qc,
        )?;
        debug_assert_eq!(pending.header.number, number);
        Ok(pending)
    }

    /// Makes a pending block final. It must extend the committed head.
    pub fn commit_pending(&self, hash: &B256) -> Result<Header> {
        let p = self.pending(hash).ok_or(ChainError::UnknownParent(*hash))?;
        let head = self.head()?;
        if p.header.parent_hash != head.hash_slow() {
            return Err(ChainError::InvalidBlock(format!(
                "block {} does not extend head {}",
                p.header.number, head.number
            )));
        }
        let number = p.header.number;
        let w = self.store.writer()?;
        let root = w.apply_bundle(number, p.executed.bundle.clone())?;
        if root != p.header.state_root {
            return Err(ChainError::Exec(format!(
                "state root changed between execution and commit at {number}"
            )));
        }
        w.put_ipld_bundle(number, &p.bundle.root, &p.bundle.blocks)?;
        w.put_block(
            &StoredBlock {
                header: p.header.clone(),
                transactions: p.executed.transactions.clone(),
                senders: p.executed.senders.clone(),
            },
            &p.executed.receipts,
        )?;
        w.prune_history(number)?;
        w.commit()?;
        // Drop pending blocks that can no longer be built upon.
        self.pending.lock().retain(|_, b| b.header.number > number);
        tracing::debug!(number, hash = %p.hash, root = %p.bundle.root, gas = p.header.gas_used, "committed block");
        Ok(p.header.clone())
    }

    /// Produces and immediately commits the next block (single-producer devnet).
    pub fn build_block(
        &self,
        candidates: impl IntoIterator<Item = (TxEnvelope, Address)>,
        timestamp: u64,
        beneficiary: Address,
    ) -> Result<BuiltBlock> {
        let head = self.head()?.hash_slow();
        let built =
            self.build_on(&head, candidates, timestamp, beneficiary, Bytes::new(), vec![])?;
        self.commit_pending(&built.hash)?;
        Ok(built)
    }

    /// Validates and imports a final block produced elsewhere. Every transaction must execute.
    pub fn import_block(&self, header: &Header, transactions: Vec<TxEnvelope>) -> Result<B256> {
        self.import_block_with_root(header, transactions, None)
    }

    /// Like [`Chain::import_block`], additionally requiring the block's IPFS envelope to hash to
    /// `root` (the CID it was announced under). The envelope's QC is taken from `qc`.
    pub fn import_block_with_root(
        &self,
        header: &Header,
        transactions: Vec<TxEnvelope>,
        root: Option<Cid>,
    ) -> Result<B256> {
        self.import_final(header, transactions, root, vec![])
    }

    /// Imports a final block whose envelope carries `qc`.
    pub fn import_final(
        &self,
        header: &Header,
        transactions: Vec<TxEnvelope>,
        root: Option<Cid>,
        qc: Vec<u8>,
    ) -> Result<B256> {
        let head = self.head()?;
        if header.parent_hash != head.hash_slow() {
            return Err(ChainError::InvalidBlock(format!("does not extend head {}", head.number)));
        }
        let pending = self.verify_block(header, transactions, qc)?;
        if let Some(exp) = root
            && exp != pending.bundle.root
        {
            self.pending.lock().remove(&pending.hash);
            return Err(ChainError::InvalidBlock(format!(
                "envelope {} != announced {exp}",
                pending.bundle.root
            )));
        }
        self.commit_pending(&pending.hash)?;
        Ok(pending.hash)
    }
}

/// Governance parameters that apply to a block (read from `ParamRegistry` in its parent state).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChainParams {
    /// Block gas limit.
    pub gas_limit: u64,
    /// Minimum base fee.
    pub min_base_fee: u64,
}

/// Length of `extra_data` in validator-produced blocks: round (8 bytes) + RANDAO reveal (96 bytes).
pub const EXTRA_DATA_LEN: usize = 8 + 96;

/// RANDAO mix of the block after `parent` (ADR 0006 §6): with a reveal in `extra_data`,
/// `keccak(parent.mix_hash ‖ keccak(reveal))`; otherwise (single-producer devnet)
/// `keccak(parent.mix_hash ‖ number)`. Validators check the reveal's signature before voting.
pub fn mix_hash(parent: &Header, extra_data: &[u8]) -> B256 {
    if extra_data.len() == EXTRA_DATA_LEN {
        keccak256([parent.mix_hash.as_slice(), keccak256(&extra_data[8..]).as_slice()].concat())
    } else {
        keccak256([parent.mix_hash.as_slice(), &(parent.number + 1).to_be_bytes()].concat())
    }
}

/// A block that was executed and verified but is not final yet.
#[derive(Debug)]
pub struct PendingBlock {
    /// Block hash.
    pub hash: B256,
    /// Header.
    pub header: Header,
    /// Execution output (transactions, receipts, state changes with reverts).
    pub executed: ExecutedBlock,
    /// Plain state changes, used to execute children before this block is final.
    pub changes: StateChangeset,
    /// IPFS representation (already in the blockstore).
    pub bundle: BlockBundle,
}

/// Serialized size of a transaction (for pool accounting).
pub fn tx_size(tx: &TxEnvelope) -> usize {
    tx.encode_2718_len()
}

/// Maximum cost a transaction may charge its sender.
pub fn max_cost(tx: &TxEnvelope) -> U256 {
    U256::from(tx.gas_limit()) * U256::from(tx.max_fee_per_gas()) + tx.value()
}

#[cfg(test)]
mod epoch_tests;
#[cfg(test)]
mod tests;
