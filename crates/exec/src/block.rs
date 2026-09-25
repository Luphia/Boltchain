//! Block execution: system calls, transactions, receipts and header assembly.

use crate::{BLOB_TX_TYPE, BlockInput, cfg_env};
use alloy_consensus::{
    Eip658Value, Header, Receipt, ReceiptEnvelope, ReceiptWithBloom, Transaction, TxEnvelope,
    TxType,
    constants::{EMPTY_OMMER_ROOT_HASH, EMPTY_ROOT_HASH},
    proofs::{calculate_receipt_root, calculate_transaction_root},
};
use alloy_eips::{
    eip1559::BaseFeeParams, eip2718::Encodable2718, eip2935::HISTORY_STORAGE_ADDRESS,
    eip4788::BEACON_ROOTS_ADDRESS, eip7685::EMPTY_REQUESTS_HASH,
};
use alloy_primitives::{Address, B64, B256, Bloom, Bytes, Log, TxKind, U256};
use bolt_primitives::params::MAX_RLP_BLOCK_SIZE;
use revm::{
    Context, DatabaseRef, ExecuteCommitEvm, MainBuilder, MainContext, SystemCallCommitEvm,
    context::{
        TxEnv,
        result::{EVMError, ExecutionResult, InvalidTransaction},
    },
    database::{BundleState, State, WrapDatabaseRef, states::bundle_state::BundleRetention},
    database_interface::bal::EvmDatabaseError,
    primitives::hardfork::SpecId,
};

/// Why a transaction could not be included.
#[derive(Debug, thiserror::Error)]
pub enum TxRejection {
    /// Blob transactions are not supported.
    #[error("blob transactions are not supported")]
    Blob,
    /// Not enough gas left in the block.
    #[error("transaction gas limit {tx} exceeds remaining block gas {remaining}")]
    BlockGasExhausted {
        /// Transaction gas limit.
        tx: u64,
        /// Gas left in the block.
        remaining: u64,
    },
    /// Including it would exceed the EIP-7934 block size limit.
    #[error("block size limit reached")]
    BlockSize,
    /// The EVM rejected it (nonce, balance, fee, chain id...).
    #[error("invalid transaction: {0}")]
    Invalid(InvalidTransaction),
}

/// Fatal execution error (database or header problems), as opposed to a rejected transaction.
#[derive(Debug, thiserror::Error)]
pub enum BlockError<E> {
    /// Database failure.
    #[error("database: {0}")]
    Database(E),
    /// A system call failed.
    #[error("system call to {0} failed")]
    SystemCall(Address),
    /// Internal EVM error.
    #[error("evm: {0}")]
    Evm(String),
}

/// Converts a signed transaction into a revm transaction environment.
pub fn tx_env(tx: &TxEnvelope, sender: Address) -> Result<TxEnv, TxRejection> {
    if tx.tx_type() == TxType::Eip4844 {
        return Err(TxRejection::Blob);
    }
    let mut env = TxEnv {
        tx_type: tx.tx_type() as u8,
        caller: sender,
        gas_limit: tx.gas_limit(),
        gas_price: tx.max_fee_per_gas(),
        gas_priority_fee: tx.max_priority_fee_per_gas(),
        kind: tx.kind(),
        value: tx.value(),
        data: tx.input().clone(),
        nonce: tx.nonce(),
        chain_id: tx.chain_id(),
        access_list: tx.access_list().cloned().unwrap_or_default(),
        ..Default::default()
    };
    if let Some(auths) = tx.authorization_list() {
        env.set_signed_authorization(auths.to_vec());
    }
    debug_assert_ne!(env.tx_type, BLOB_TX_TYPE);
    Ok(env)
}

/// EIP-1559 base fee of the block after `parent`, floored at the protocol minimum.
pub fn next_base_fee(parent: &Header, min_base_fee: u64) -> u64 {
    let next = BaseFeeParams::ethereum().next_block_base_fee(
        parent.gas_used,
        parent.gas_limit,
        parent.base_fee_per_gas.unwrap_or(min_base_fee),
    );
    next.max(min_base_fee)
}

/// Header fields decided before execution.
#[derive(Debug, Clone)]
pub struct BlockParams {
    /// EVM inputs (number, timestamp, beneficiary, gas limit, base fee, prevrandao).
    pub input: BlockInput,
    /// Hash of the parent block.
    pub parent_hash: B256,
    /// Parent QC hash (EIP-4788 slot). Zero until consensus lands in M3.
    pub parent_beacon_root: B256,
    /// Header extra data.
    pub extra_data: Bytes,
    /// Header difficulty: the PoW difficulty of a mined block, zero under PoS.
    pub difficulty: U256,
    /// Header nonce: the PoW seal of a mined block (chosen after execution), zero under PoS.
    pub nonce: B64,
}

/// The output of executing a block, before the state root is known.
#[derive(Debug)]
pub struct ExecutedBlock {
    /// Block parameters.
    pub params: BlockParams,
    /// Included transactions.
    pub transactions: Vec<TxEnvelope>,
    /// Their senders.
    pub senders: Vec<Address>,
    /// One receipt per transaction.
    pub receipts: Vec<ReceiptEnvelope>,
    /// Total gas used.
    pub gas_used: u64,
    /// State changes of this block, with reverts for history.
    pub bundle: BundleState,
}

impl ExecutedBlock {
    /// Builds the header once the post-state root is known.
    pub fn header(&self, state_root: B256) -> Header {
        let p = &self.params;
        let bloom = self.receipts.iter().fold(Bloom::ZERO, |mut acc, r| {
            acc.accrue_bloom(r.logs_bloom());
            acc
        });
        Header {
            parent_hash: p.parent_hash,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: p.input.beneficiary,
            state_root,
            transactions_root: calculate_transaction_root(&self.transactions),
            receipts_root: calculate_receipt_root(&self.receipts),
            logs_bloom: bloom,
            difficulty: p.difficulty,
            number: p.input.number,
            gas_limit: p.input.gas_limit,
            gas_used: self.gas_used,
            timestamp: p.input.timestamp,
            extra_data: p.extra_data.clone(),
            mix_hash: p.input.prevrandao,
            nonce: p.nonce,
            base_fee_per_gas: Some(p.input.base_fee),
            withdrawals_root: Some(EMPTY_ROOT_HASH),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            parent_beacon_block_root: Some(p.parent_beacon_root),
            requests_hash: Some(EMPTY_REQUESTS_HASH),
            block_access_list_hash: None,
            slot_number: None,
        }
    }
}

type Db<D> = State<WrapDatabaseRef<D>>;

/// Executes transactions one by one on top of a parent state.
pub struct BlockExecutor<D: DatabaseRef> {
    state: Db<D>,
    params: BlockParams,
    transactions: Vec<TxEnvelope>,
    senders: Vec<Address>,
    receipts: Vec<ReceiptEnvelope>,
    gas_used: u64,
    size: usize,
}

impl<D: DatabaseRef> std::fmt::Debug for BlockExecutor<D> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BlockExecutor")
            .field("number", &self.params.input.number)
            .field("txs", &self.transactions.len())
            .field("gas_used", &self.gas_used)
            .finish()
    }
}

/// Upper bound for the header plus list overhead when checking EIP-7934.
const HEADER_SIZE_ALLOWANCE: usize = 1024;

impl<D: DatabaseRef> BlockExecutor<D>
where
    D::Error: std::error::Error + Send + Sync + 'static,
{
    /// Starts a block and runs the pre-block system calls (EIP-4788, EIP-2935).
    pub fn new(db: D, params: BlockParams) -> Result<Self, BlockError<D::Error>> {
        let state = State::builder().with_database_ref(db).with_bundle_update().build();
        let mut this = Self {
            state,
            params,
            transactions: Vec::new(),
            senders: Vec::new(),
            receipts: Vec::new(),
            gas_used: 0,
            size: HEADER_SIZE_ALLOWANCE,
        };
        if this.params.input.number > 0 {
            let beacon = this.params.parent_beacon_root;
            let parent = this.params.parent_hash;
            this.system_call(BEACON_ROOTS_ADDRESS, beacon)?;
            this.system_call(HISTORY_STORAGE_ADDRESS, parent)?;
        }
        Ok(this)
    }

    fn system_call(&mut self, to: Address, arg: B256) -> Result<(), BlockError<D::Error>> {
        let block = self.params.input.block_env();
        let mut evm = Context::mainnet()
            .with_db(&mut self.state)
            .with_cfg(cfg_env(self.params.input.chain_id))
            .with_block(block)
            .build_mainnet();
        let res =
            evm.system_call_commit(to, Bytes::copy_from_slice(arg.as_slice())).map_err(map_evm)?;
        if !res.is_success() {
            return Err(BlockError::SystemCall(to));
        }
        Ok(())
    }

    /// Calls a system contract as the system address (no gas charged to the block, no nonce) and
    /// commits the result. Returns the output; a revert is a fatal block error.
    pub fn system_call_data(
        &mut self,
        to: Address,
        data: Bytes,
    ) -> Result<Bytes, BlockError<D::Error>> {
        let block = self.params.input.block_env();
        let mut evm = Context::mainnet()
            .with_db(&mut self.state)
            .with_cfg(cfg_env(self.params.input.chain_id))
            .with_block(block)
            .build_mainnet();
        match evm.system_call_commit(to, data).map_err(map_evm)? {
            ExecutionResult::Success { output, .. } => Ok(output.into_data()),
            _ => Err(BlockError::SystemCall(to)),
        }
    }

    /// Read-only call against the current block state (nothing is committed). Runs without the
    /// EIP-7825 per-transaction gas cap so views over large validator sets fit.
    pub fn view_call(&mut self, to: Address, data: Bytes) -> Result<Bytes, BlockError<D::Error>> {
        view_call(&mut self.state, &self.params.input, to, data).map_err(|e| match e {
            ViewError::Database(d) => db_err(d),
            ViewError::Reverted => BlockError::SystemCall(to),
            ViewError::Evm(s) => BlockError::Evm(s),
        })
    }

    /// Credits newly issued BOLT to `to` (epoch emission). Not a transaction; recorded in the
    /// block's state changes.
    pub fn credit(&mut self, to: Address, amount: u128) -> Result<(), BlockError<D::Error>> {
        use revm::database_interface::DatabaseCommitExt;
        self.state.increment_balances([(to, amount)]).map_err(db_err)
    }

    /// Irregular state change of a hard fork (ADR 0008 §2): replaces `address`'s code and/or
    /// writes storage slots, before the block's system calls. Recorded in the block's state
    /// changes like any other write.
    pub fn apply_irregular(
        &mut self,
        address: Address,
        code: Option<Bytes>,
        storage: &[(U256, U256)],
    ) -> Result<(), BlockError<D::Error>> {
        use revm::{
            Database, DatabaseCommit,
            state::{Account, EvmStorageSlot},
        };
        let mut info = self.state.basic(address).map_err(db_err)?.unwrap_or_default();
        if let Some(code) = code {
            let bytecode = revm::bytecode::Bytecode::new_raw(code);
            info.code_hash = bytecode.hash_slow();
            info.code = Some(bytecode);
            info.nonce = info.nonce.max(1);
        }
        let mut account = Account::from(info);
        for (slot, value) in storage {
            let original = self.state.storage(address, *slot).map_err(db_err)?;
            account.storage.insert(
                *slot,
                EvmStorageSlot::new_changed(original, *value, revm::state::TransactionId::ZERO),
            );
        }
        account.mark_touch();
        self.state.commit([(address, account)].into_iter().collect());
        Ok(())
    }

    /// Block parameters.
    pub fn params(&self) -> &BlockParams {
        &self.params
    }

    /// Gas left in the block.
    pub fn gas_remaining(&self) -> u64 {
        self.params.input.gas_limit - self.gas_used
    }

    /// Number of included transactions.
    pub fn tx_count(&self) -> usize {
        self.transactions.len()
    }

    /// Tries to execute and include `tx`. A rejected transaction leaves the state untouched.
    pub fn add(
        &mut self,
        tx: TxEnvelope,
        sender: Address,
    ) -> Result<Result<&ReceiptEnvelope, TxRejection>, BlockError<D::Error>> {
        let env = match tx_env(&tx, sender) {
            Ok(e) => e,
            Err(r) => return Ok(Err(r)),
        };
        if env.gas_limit > self.gas_remaining() {
            return Ok(Err(TxRejection::BlockGasExhausted {
                tx: env.gas_limit,
                remaining: self.gas_remaining(),
            }));
        }
        let tx_size = tx.encode_2718_len() + 8;
        if self.size + tx_size > MAX_RLP_BLOCK_SIZE {
            return Ok(Err(TxRejection::BlockSize));
        }

        let block = self.params.input.block_env();
        let mut evm = Context::mainnet()
            .with_db(&mut self.state)
            .with_cfg(cfg_env(self.params.input.chain_id))
            .with_block(block)
            .build_mainnet();
        let result = match evm.transact_commit(env) {
            Ok(r) => r,
            Err(EVMError::Transaction(e)) => return Ok(Err(TxRejection::Invalid(e))),
            Err(e) => return Err(map_evm(e)),
        };

        let gas = result.tx_gas_used();
        self.gas_used += gas;
        self.size += tx_size;
        let (success, logs) = match result {
            ExecutionResult::Success { logs, .. } => (true, logs),
            ExecutionResult::Revert { .. } | ExecutionResult::Halt { .. } => (false, Vec::new()),
        };
        let receipt = Receipt {
            status: Eip658Value::Eip658(success),
            cumulative_gas_used: self.gas_used,
            logs: logs.into_iter().collect::<Vec<Log>>(),
        };
        let with_bloom = ReceiptWithBloom::from(receipt);
        let envelope = match tx.tx_type() {
            TxType::Legacy => ReceiptEnvelope::Legacy(with_bloom),
            TxType::Eip2930 => ReceiptEnvelope::Eip2930(with_bloom),
            TxType::Eip1559 => ReceiptEnvelope::Eip1559(with_bloom),
            TxType::Eip7702 => ReceiptEnvelope::Eip7702(with_bloom),
            TxType::Eip4844 => unreachable!("rejected above"),
        };
        self.transactions.push(tx);
        self.senders.push(sender);
        self.receipts.push(envelope);
        Ok(Ok(self.receipts.last().expect("just pushed")))
    }

    /// Finishes the block and returns its state changes.
    pub fn finish(mut self) -> ExecutedBlock {
        self.state.merge_transitions(BundleRetention::Reverts);
        ExecutedBlock {
            params: self.params,
            transactions: self.transactions,
            senders: self.senders,
            receipts: self.receipts,
            gas_used: self.gas_used,
            bundle: self.state.take_bundle(),
        }
    }
}

/// Why a view call failed.
#[derive(Debug)]
pub enum ViewError<E> {
    /// Database failure.
    Database(E),
    /// The call reverted or halted.
    Reverted,
    /// Internal EVM error.
    Evm(String),
}

/// Gas available to a view call.
pub const VIEW_GAS: u64 = 1 << 40;

/// Read-only call from the system address against `db`, with no gas cap, no fee and no nonce check.
pub fn view_call<DB: revm::Database>(
    db: DB,
    input: &BlockInput,
    to: Address,
    data: Bytes,
) -> Result<Bytes, ViewError<DB::Error>> {
    use revm::ExecuteEvm;
    let mut block = input.block_env();
    block.basefee = 0;
    block.gas_limit = VIEW_GAS;
    let mut cfg = cfg_env(input.chain_id);
    cfg.disable_nonce_check = true;
    cfg.tx_gas_limit_cap = Some(VIEW_GAS);
    let tx = TxEnv {
        caller: revm::handler::SYSTEM_ADDRESS,
        kind: TxKind::Call(to),
        data,
        gas_limit: VIEW_GAS,
        gas_price: 0,
        gas_priority_fee: None,
        chain_id: Some(input.chain_id),
        ..Default::default()
    };
    let mut evm = Context::mainnet().with_db(db).with_cfg(cfg).with_block(block).build_mainnet();
    match evm.transact(tx) {
        Ok(r) => match r.result {
            ExecutionResult::Success { output, .. } => Ok(output.into_data()),
            _ => Err(ViewError::Reverted),
        },
        Err(EVMError::Database(d)) => Err(ViewError::Database(d)),
        Err(e) => Err(ViewError::Evm(format!("{e:?}"))),
    }
}

fn db_err<E: std::fmt::Debug>(e: EvmDatabaseError<E>) -> BlockError<E> {
    match e {
        EvmDatabaseError::Database(d) => BlockError::Database(d),
        other => BlockError::Evm(format!("{other:?}")),
    }
}

fn map_evm<E: std::fmt::Debug>(e: EVMError<EvmDatabaseError<E>>) -> BlockError<E> {
    match e {
        EVMError::Database(EvmDatabaseError::Database(d)) => BlockError::Database(d),
        other => BlockError::Evm(format!("{other:?}")),
    }
}

/// Address of a contract created by `sender` with `nonce` (CREATE).
pub fn create_address(sender: Address, nonce: u64) -> Address {
    sender.create(nonce)
}

/// Whether a transaction deploys a contract.
pub fn is_create(tx: &TxEnvelope) -> bool {
    matches!(tx.kind(), TxKind::Create)
}

/// Compile-time guard: Boltchain always runs Osaka.
const _: () = assert!(matches!(crate::SPEC, SpecId::OSAKA));
