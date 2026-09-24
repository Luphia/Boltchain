//! Boltchain execution layer.
//!
//! Every block runs under `SpecId::OSAKA` from genesis; there is no hard-fork switching logic.
//! Blob transactions (EIP-4844, type 3) are rejected before they reach the EVM, and the blob
//! header fields stay at zero, so no KZG setup or PeerDAS is needed.

pub mod block;

pub use block::{BlockExecutor, BlockParams, ExecutedBlock, TxRejection, next_base_fee, tx_env};

use revm::{
    Context, Database, ExecuteEvm, MainBuilder, MainContext,
    context::{
        BlockEnv, CfgEnv, TxEnv,
        result::{EVMError, ExecResultAndState, ExecutionResult},
    },
    primitives::{Address, B256, U256, hardfork::SpecId},
};

/// The only EVM spec Boltchain runs.
pub const SPEC: SpecId = SpecId::OSAKA;

/// EIP-2718 type byte of blob transactions.
pub const BLOB_TX_TYPE: u8 = 3;

/// EVM configuration: Osaka with the mainnet gas schedule.
///
/// `chain_id` is [`bolt_primitives::params::CHAIN_ID`] (8017) on Boltchain itself; local dev chains use their own id.
pub fn cfg_env(chain_id: u64) -> CfgEnv {
    let mut cfg = CfgEnv::new_with_spec(SPEC);
    cfg.chain_id = chain_id;
    cfg
}

/// Per-block inputs to the EVM.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BlockInput {
    /// EIP-155 chain id.
    pub chain_id: u64,
    /// Block height.
    pub number: u64,
    /// Unix timestamp.
    pub timestamp: u64,
    /// Fee recipient (the slot leader).
    pub beneficiary: Address,
    /// Block gas limit.
    pub gas_limit: u64,
    /// EIP-1559 base fee.
    pub base_fee: u64,
    /// PREVRANDAO: the epoch randomness mixed with the leader's BLS-VRF output.
    pub prevrandao: B256,
}

impl BlockInput {
    /// Builds the revm block environment. Blob excess gas is always zero.
    pub fn block_env(&self) -> BlockEnv {
        let mut env = BlockEnv {
            number: U256::from(self.number),
            beneficiary: self.beneficiary,
            timestamp: U256::from(self.timestamp),
            gas_limit: self.gas_limit,
            basefee: self.base_fee,
            difficulty: U256::ZERO,
            prevrandao: Some(self.prevrandao),
            ..Default::default()
        };
        let fraction = cfg_env(self.chain_id).blob_base_fee_update_fraction();
        env.set_blob_excess_gas_and_price(0, fraction);
        env
    }
}

/// Why a transaction was not executed.
#[derive(Debug, thiserror::Error)]
pub enum ExecError<DbErr> {
    /// Boltchain does not accept blob transactions.
    #[error("blob transactions (type 3) are not supported on Boltchain")]
    BlobTxRejected,
    /// The EVM rejected the transaction or the database failed.
    #[error("evm: {0}")]
    Evm(#[from] EVMError<DbErr>),
}

/// Result of executing one transaction, with the state changes not yet committed.
pub type TxOutcome = ExecResultAndState<ExecutionResult>;

/// Executes one transaction against `db` without committing.
pub fn transact<DB: Database>(
    db: DB,
    block: &BlockInput,
    tx: TxEnv,
) -> Result<TxOutcome, ExecError<DB::Error>> {
    if tx.tx_type == BLOB_TX_TYPE || !tx.blob_hashes.is_empty() {
        return Err(ExecError::BlobTxRejected);
    }
    let mut evm = Context::mainnet()
        .with_db(db)
        .with_cfg(cfg_env(block.chain_id))
        .with_block(block.block_env())
        .build_mainnet();
    Ok(evm.transact(tx)?)
}

#[cfg(test)]
mod block_tests;
#[cfg(test)]
mod tests;
