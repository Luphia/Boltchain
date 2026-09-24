//! `eth_call` and `eth_estimateGas`.

use crate::{RpcContext, RpcResult, err, internal};
use alloy_eips::BlockId;
use alloy_primitives::{Bytes, TxKind, U256};
use alloy_rpc_types_eth::TransactionRequest;
use bolt_exec::{BlockInput, cfg_env};
use bolt_store::{RO, StateView, Tx};
use jsonrpsee::types::ErrorObjectOwned;
use revm::{
    Context, ExecuteEvm, MainBuilder, MainContext,
    context::{
        TxEnv,
        result::{ExecutionResult, Output},
    },
    database::WrapDatabaseRef,
};

/// EIP-7825 per-transaction gas cap.
const TX_GAS_CAP: u64 = 1 << 24;

enum CallOutcome {
    Success { gas_used: u64, output: Bytes },
    Revert { output: Bytes },
    Halt { reason: String },
}

fn revert_error(output: &Bytes) -> ErrorObjectOwned {
    ErrorObjectOwned::owned(3, "execution reverted", Some(output.clone()))
}

fn run(
    ctx: &RpcContext,
    r: &Tx<'_, RO>,
    at: Option<u64>,
    req: &TransactionRequest,
    gas: u64,
) -> RpcResult<CallOutcome> {
    let view: StateView<'_, '_, RO> = ctx.view(r, at)?;
    let number = match at {
        Some(n) => n,
        None => r.head().map_err(internal)?.unwrap_or(0),
    };
    let header = r.header(number).map_err(internal)?.ok_or_else(|| internal("missing header"))?;
    let cfg_chain = ctx.chain.config();

    let gas_price = req.gas_price.or(req.max_fee_per_gas);
    // With no fee given, run with a zero base fee like geth does for calls.
    let base_fee =
        if gas_price.is_some() { header.base_fee_per_gas.unwrap_or_default() } else { 0 };
    let block = BlockInput {
        chain_id: cfg_chain.chain_id,
        number: number + 1,
        timestamp: header.timestamp + cfg_chain.slot_seconds,
        beneficiary: header.beneficiary,
        gas_limit: cfg_chain.gas_limit,
        base_fee,
        prevrandao: header.mix_hash,
    };

    let mut tx = TxEnv {
        caller: req.from.unwrap_or_default(),
        gas_limit: gas,
        gas_price: gas_price.unwrap_or(0),
        gas_priority_fee: req.max_priority_fee_per_gas,
        kind: req.to.unwrap_or(TxKind::Create),
        value: req.value.unwrap_or(U256::ZERO),
        data: req.input.input().cloned().unwrap_or_default(),
        nonce: req.nonce.unwrap_or_default(),
        chain_id: Some(cfg_chain.chain_id),
        access_list: req.access_list.clone().unwrap_or_default(),
        ..Default::default()
    };
    if let Some(auths) = &req.authorization_list {
        tx.set_signed_authorization(auths.clone());
    }
    if req.blob_versioned_hashes.as_ref().is_some_and(|h| !h.is_empty()) {
        return Err(err(-32000, "blob transactions are not supported on Boltchain"));
    }
    let _ = tx.derive_tx_type();

    let mut cfg = cfg_env(cfg_chain.chain_id);
    cfg.disable_nonce_check = true;
    let mut evm = Context::mainnet()
        .with_db(WrapDatabaseRef(&view))
        .with_cfg(cfg)
        .with_block(block.block_env())
        .build_mainnet();
    let res = evm.transact(tx).map_err(|e| err(-32000, format!("{e:?}")))?;
    let gas_used = res.result.tx_gas_used();
    Ok(match res.result {
        ExecutionResult::Success { output, .. } => {
            let output = match output {
                Output::Call(b) => b,
                Output::Create(b, _) => b,
            };
            CallOutcome::Success { gas_used, output }
        }
        ExecutionResult::Revert { output, .. } => CallOutcome::Revert { output },
        ExecutionResult::Halt { reason, .. } => CallOutcome::Halt { reason: format!("{reason:?}") },
    })
}

fn gas_cap(ctx: &RpcContext) -> u64 {
    ctx.chain.config().gas_limit.min(TX_GAS_CAP)
}

/// `eth_call`.
pub(crate) fn eth_call(
    ctx: &RpcContext,
    req: TransactionRequest,
    id: Option<BlockId>,
) -> RpcResult<Bytes> {
    let r = ctx.chain.store().reader().map_err(internal)?;
    let at = ctx.resolve(&r, id)?;
    let gas = req.gas.unwrap_or(gas_cap(ctx)).min(gas_cap(ctx));
    match run(ctx, &r, at, &req, gas)? {
        CallOutcome::Success { output, .. } => Ok(output),
        CallOutcome::Revert { output } => Err(revert_error(&output)),
        CallOutcome::Halt { reason } => Err(err(-32000, format!("execution halted: {reason}"))),
    }
}

/// `eth_estimateGas`: binary search for the lowest gas limit that succeeds.
pub(crate) fn estimate_gas(
    ctx: &RpcContext,
    req: TransactionRequest,
    id: Option<BlockId>,
) -> RpcResult<u64> {
    let r = ctx.chain.store().reader().map_err(internal)?;
    let at = ctx.resolve(&r, id)?;
    let mut hi = req.gas.unwrap_or(gas_cap(ctx)).min(gas_cap(ctx));
    let used = match run(ctx, &r, at, &req, hi)? {
        CallOutcome::Success { gas_used, .. } => gas_used,
        CallOutcome::Revert { output } => return Err(revert_error(&output)),
        CallOutcome::Halt { reason } => {
            return Err(err(-32000, format!("gas required exceeds allowance ({reason})")));
        }
    };
    // Gas refunds and the 63/64 rule mean `used` may be too low; search [used, hi].
    let mut lo = used.saturating_sub(1).max(20_999);
    if matches!(run(ctx, &r, at, &req, used)?, CallOutcome::Success { .. }) {
        return Ok(used);
    }
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        match run(ctx, &r, at, &req, mid)? {
            CallOutcome::Success { .. } => hi = mid,
            _ => lo = mid,
        }
    }
    Ok(hi)
}
