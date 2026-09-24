//! Chain state machine: genesis initialisation, block production and block import.
//!
//! Execution reads a consistent snapshot of the store while RPC readers keep running; the only
//! write transaction is the final commit of each block (state, tries, block, history pruning).

use alloy_consensus::{Header, Transaction as _, TxEnvelope, transaction::SignerRecoverable};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use bolt_exec::{
    BlockExecutor, BlockInput, BlockParams, ExecutedBlock, TxRejection, next_base_fee,
};
use bolt_ipld::{BlockBundle, Cid};
use bolt_primitives::{Genesis, genesis::ChainConfig};
use bolt_store::{InitAccount, StateView, Store, StoreError, StoredBlock};
use std::{collections::BTreeMap, path::Path};

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
    genesis_hash: B256,
}

impl Chain {
    /// Opens the datadir, writing genesis on first start.
    pub fn open(datadir: impl AsRef<Path>, genesis: &Genesis) -> Result<Self> {
        let store = Store::open(datadir)?;
        let expected = genesis.hash();
        let head = store.reader()?.head()?;
        match head {
            None => {
                let alloc: BTreeMap<Address, InitAccount> = genesis
                    .effective_alloc()
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
                let header = genesis.header();
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
        Ok(Self { store, config: genesis.config.clone(), genesis_hash: expected })
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

    /// Base fee the next block will use.
    pub fn next_base_fee(&self) -> Result<u64> {
        Ok(next_base_fee(&self.head()?, self.config.min_base_fee_wei))
    }

    fn params_for(&self, parent: &Header, timestamp: u64, beneficiary: Address) -> BlockParams {
        let number = parent.number + 1;
        // Placeholder randomness until the BLS-VRF lands in M4.
        let prevrandao = keccak256([parent.mix_hash.as_slice(), &number.to_be_bytes()].concat());
        BlockParams {
            input: BlockInput {
                chain_id: self.config.chain_id,
                number,
                timestamp,
                beneficiary,
                gas_limit: self.config.gas_limit,
                base_fee: next_base_fee(parent, self.config.min_base_fee_wei),
                prevrandao,
            },
            parent_hash: parent.hash_slow(),
            parent_beacon_root: B256::ZERO,
            extra_data: Bytes::new(),
        }
    }

    /// Produces the next block from candidate transactions (already sender-recovered), skipping
    /// any that do not fit or fail validation.
    pub fn build_block(
        &self,
        candidates: impl IntoIterator<Item = (TxEnvelope, Address)>,
        timestamp: u64,
        beneficiary: Address,
    ) -> Result<BuiltBlock> {
        let parent = self.head()?;
        let timestamp = timestamp.max(parent.timestamp + 1);
        let params = self.params_for(&parent, timestamp, beneficiary);
        let mut included = Vec::new();
        let mut rejected = Vec::new();
        let executed = {
            let reader = self.store.reader()?;
            let view = StateView::latest(&reader);
            let mut ex =
                BlockExecutor::new(&view, params).map_err(|e| ChainError::Exec(e.to_string()))?;
            for (tx, sender) in candidates {
                if ex.gas_remaining() < 21_000 {
                    break;
                }
                let hash = *tx.tx_hash();
                match ex.add(tx, sender).map_err(|e| ChainError::Exec(e.to_string()))? {
                    Ok(_) => included.push(hash),
                    Err(e) => {
                        let permanent = matches!(e, TxRejection::Invalid(_) | TxRejection::Blob);
                        rejected.push((hash, e.to_string(), permanent));
                    }
                }
            }
            ex.finish()
        };
        let (header, bundle) = self.commit(executed, None, None)?;
        let hash = header.hash_slow();
        Ok(BuiltBlock { header, hash, included, rejected, bundle })
    }

    /// Validates and imports a block produced elsewhere. Every transaction must execute.
    pub fn import_block(&self, header: &Header, transactions: Vec<TxEnvelope>) -> Result<B256> {
        self.import_block_with_root(header, transactions, None)
    }

    /// Like [`Chain::import_block`], additionally requiring the block's IPFS envelope to hash to
    /// `root` (the CID it was announced under).
    pub fn import_block_with_root(
        &self,
        header: &Header,
        transactions: Vec<TxEnvelope>,
        root: Option<Cid>,
    ) -> Result<B256> {
        let parent = self.head()?;
        let invalid = |m: String| ChainError::InvalidBlock(m);
        if header.parent_hash != parent.hash_slow() || header.number != parent.number + 1 {
            return Err(invalid(format!("does not extend head {}", parent.number)));
        }
        if header.timestamp <= parent.timestamp {
            return Err(invalid("timestamp not after parent".into()));
        }
        let mut params = self.params_for(&parent, header.timestamp, header.beneficiary);
        if header.base_fee_per_gas != Some(params.input.base_fee) {
            return Err(invalid("wrong base fee".into()));
        }
        if header.gas_limit != params.input.gas_limit {
            return Err(invalid("wrong gas limit".into()));
        }
        params.input.prevrandao = header.mix_hash;
        params.parent_beacon_root = header.parent_beacon_block_root.unwrap_or_default();
        params.extra_data = header.extra_data.clone();

        // Signature recovery dominates import time for simple transactions: do it on all cores.
        let senders: Vec<Address> = {
            use rayon::prelude::*;
            transactions
                .par_iter()
                .map(|tx| tx.recover_signer())
                .collect::<Result<_, _>>()
                .map_err(|_| invalid("bad signature".into()))?
        };
        let executed = {
            let reader = self.store.reader()?;
            let view = StateView::latest(&reader);
            let mut ex =
                BlockExecutor::new(&view, params).map_err(|e| ChainError::Exec(e.to_string()))?;
            for (tx, sender) in transactions.into_iter().zip(senders) {
                let hash = *tx.tx_hash();
                ex.add(tx, sender)
                    .map_err(|e| ChainError::Exec(e.to_string()))?
                    .map_err(|e: TxRejection| invalid(format!("tx {hash}: {e}")))?;
            }
            ex.finish()
        };
        let (sealed, _) = self.commit(executed, Some(header), root)?;
        Ok(sealed.hash_slow())
    }

    /// Applies an executed block. With `expected`, the resulting header must match exactly or
    /// nothing is written.
    fn commit(
        &self,
        executed: ExecutedBlock,
        expected: Option<&Header>,
        expected_root: Option<Cid>,
    ) -> Result<(Header, BlockBundle)> {
        let number = executed.params.input.number;
        let w = self.store.writer()?;
        let root = w.apply_bundle(number, executed.bundle.clone())?;
        let header = executed.header(root);
        if let Some(exp) = expected
            && exp != &header
        {
            // Dropping `w` aborts the write transaction.
            return Err(ChainError::InvalidBlock(format!(
                "header mismatch (state root {} vs {})",
                header.state_root, exp.state_root
            )));
        }
        let parent_root = w.envelope_root(number - 1)?;
        let bundle = bolt_ipld::bundle(&header, &executed.transactions, parent_root, vec![]);
        if let Some(exp) = expected_root
            && exp != bundle.root
        {
            return Err(ChainError::InvalidBlock(format!(
                "envelope {} != announced {exp}",
                bundle.root
            )));
        }
        w.put_ipld_bundle(number, &bundle.root, &bundle.blocks)?;
        w.put_block(
            &StoredBlock {
                header: header.clone(),
                transactions: executed.transactions,
                senders: executed.senders,
            },
            &executed.receipts,
        )?;
        w.prune_history(number)?;
        w.commit()?;
        tracing::debug!(number, hash = %header.hash_slow(), root = %bundle.root, gas = header.gas_used, "committed block");
        Ok((header, bundle))
    }
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
mod tests;
