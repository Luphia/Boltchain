//! Store records -> JSON-RPC response objects.

use crate::{RpcResult, err, internal};
use alloy_consensus::{ReceiptEnvelope, Transaction as _, TxEnvelope, transaction::Recovered};
use alloy_eips::eip2718::Encodable2718;
use alloy_primitives::{Address, B256, U256};
use alloy_rpc_types_eth::{
    Block, BlockTransactions, Filter, Header as RpcHeader, Log, Transaction, TransactionReceipt,
};
use bolt_store::{RO, StoredBlock, Tx};
use serde_json::Value;

fn to_json<T: serde::Serialize>(v: T) -> RpcResult<Value> {
    serde_json::to_value(v).map_err(internal)
}

fn rpc_tx(block: &StoredBlock, hash: B256, i: usize) -> Transaction {
    let tx = block.transactions[i].clone();
    let base_fee = block.header.base_fee_per_gas;
    Transaction {
        effective_gas_price: Some(tx.effective_gas_price(base_fee)),
        inner: Recovered::new_unchecked(tx, block.senders[i]),
        block_hash: Some(hash),
        block_number: Some(block.header.number),
        transaction_index: Some(i as u64),
        block_timestamp: Some(block.header.timestamp),
    }
}

/// `eth_getBlockBy*` response.
pub(crate) fn block_json(r: &Tx<'_, RO>, number: u64, full: bool) -> RpcResult<Option<Value>> {
    let Some(block) = r.block(number).map_err(internal)? else { return Ok(None) };
    let hash = block.header.hash_slow();
    let size: usize = alloy_rlp::Encodable::length(&block.header)
        + block.transactions.iter().map(|t| t.encode_2718_len()).sum::<usize>()
        + 8;
    let transactions = if full {
        BlockTransactions::Full(
            (0..block.transactions.len()).map(|i| rpc_tx(&block, hash, i)).collect(),
        )
    } else {
        BlockTransactions::Hashes(block.transactions.iter().map(|t| *t.tx_hash()).collect())
    };
    let header = RpcHeader {
        hash,
        inner: block.header.clone(),
        total_difficulty: Some(U256::ZERO),
        size: Some(U256::from(size)),
    };
    let rpc: Block<Transaction, RpcHeader> =
        Block { header, uncles: vec![], transactions, withdrawals: Some(Default::default()) };
    to_json(rpc).map(Some)
}

/// A mined transaction by hash.
pub(crate) fn mined_tx_json(r: &Tx<'_, RO>, hash: &B256) -> RpcResult<Option<Value>> {
    let Some((n, i)) = r.tx_location(hash).map_err(internal)? else { return Ok(None) };
    let block = r.block(n).map_err(internal)?.ok_or_else(|| internal("indexed block missing"))?;
    let bh = block.header.hash_slow();
    to_json(rpc_tx(&block, bh, i as usize)).map(Some)
}

/// A pooled (not yet mined) transaction.
pub(crate) fn pending_tx_json(tx: &TxEnvelope, sender: Address) -> Value {
    let t = Transaction {
        inner: Recovered::new_unchecked(tx.clone(), sender),
        block_hash: None,
        block_number: None,
        transaction_index: None,
        effective_gas_price: None,
        block_timestamp: None,
    };
    serde_json::to_value(t).unwrap_or(Value::Null)
}

fn receipts_of(r: &Tx<'_, RO>, block: &StoredBlock) -> RpcResult<Vec<TransactionReceipt>> {
    let n = block.header.number;
    let hash = block.header.hash_slow();
    let receipts = r.receipts(n).map_err(internal)?.unwrap_or_default();
    if receipts.len() != block.transactions.len() {
        return Err(internal("receipt count mismatch"));
    }
    let mut out = Vec::with_capacity(receipts.len());
    let mut prev_cumulative = 0u64;
    let mut log_index = 0u64;
    for (i, (tx, receipt)) in block.transactions.iter().zip(receipts).enumerate() {
        let sender = block.senders[i];
        let gas_used = receipt.cumulative_gas_used() - prev_cumulative;
        prev_cumulative = receipt.cumulative_gas_used();
        let tx_hash = *tx.tx_hash();
        let contract_address = tx.kind().is_create().then(|| sender.create(tx.nonce()));
        let map_logs = |rwb: alloy_consensus::ReceiptWithBloom<alloy_consensus::Receipt>,
                        log_index: &mut u64| {
            let logs = rwb
                .receipt
                .logs
                .into_iter()
                .map(|inner| {
                    let l = Log {
                        inner,
                        block_hash: Some(hash),
                        block_number: Some(n),
                        block_timestamp: Some(block.header.timestamp),
                        transaction_hash: Some(tx_hash),
                        transaction_index: Some(i as u64),
                        log_index: Some(*log_index),
                        removed: false,
                    };
                    *log_index += 1;
                    l
                })
                .collect();
            alloy_consensus::ReceiptWithBloom {
                receipt: alloy_consensus::Receipt {
                    status: rwb.receipt.status,
                    cumulative_gas_used: rwb.receipt.cumulative_gas_used,
                    logs,
                },
                logs_bloom: rwb.logs_bloom,
            }
        };
        let inner = match receipt {
            ReceiptEnvelope::Legacy(x) => ReceiptEnvelope::Legacy(map_logs(x, &mut log_index)),
            ReceiptEnvelope::Eip2930(x) => ReceiptEnvelope::Eip2930(map_logs(x, &mut log_index)),
            ReceiptEnvelope::Eip1559(x) => ReceiptEnvelope::Eip1559(map_logs(x, &mut log_index)),
            ReceiptEnvelope::Eip7702(x) => ReceiptEnvelope::Eip7702(map_logs(x, &mut log_index)),
            ReceiptEnvelope::Eip4844(_) => return Err(internal("blob receipt in store")),
        };
        out.push(TransactionReceipt {
            inner,
            transaction_hash: tx_hash,
            transaction_index: Some(i as u64),
            block_hash: Some(hash),
            block_number: Some(n),
            gas_used,
            effective_gas_price: tx.effective_gas_price(block.header.base_fee_per_gas),
            blob_gas_used: None,
            blob_gas_price: None,
            from: sender,
            to: tx.to(),
            contract_address,
        });
    }
    Ok(out)
}

/// `eth_getTransactionReceipt`.
pub(crate) fn receipt_json(r: &Tx<'_, RO>, hash: &B256) -> RpcResult<Option<Value>> {
    let Some((n, i)) = r.tx_location(hash).map_err(internal)? else { return Ok(None) };
    let block = r.block(n).map_err(internal)?.ok_or_else(|| internal("indexed block missing"))?;
    let mut all = receipts_of(r, &block)?;
    to_json(all.swap_remove(i as usize)).map(Some)
}

/// `eth_getBlockReceipts`.
pub(crate) fn block_receipts_json(r: &Tx<'_, RO>, n: u64) -> RpcResult<Option<Vec<Value>>> {
    let Some(block) = r.block(n).map_err(internal)? else { return Ok(None) };
    receipts_of(r, &block)?.into_iter().map(to_json).collect::<RpcResult<Vec<_>>>().map(Some)
}

/// `eth_getLogs`.
pub(crate) fn logs(r: &Tx<'_, RO>, filter: &Filter) -> RpcResult<Vec<Value>> {
    let head = r.head().map_err(internal)?.unwrap_or(0);
    let (from, to) = if let Some(h) = filter.get_block_hash() {
        let n =
            r.block_number(&h).map_err(internal)?.ok_or_else(|| err(-32001, "block not found"))?;
        (n, n)
    } else {
        (filter.get_from_block().unwrap_or(head), filter.get_to_block().unwrap_or(head).min(head))
    };
    if to < from {
        return Ok(vec![]);
    }
    if to - from > crate::MAX_LOG_RANGE {
        return Err(err(-32005, format!("block range too large (max {})", crate::MAX_LOG_RANGE)));
    }
    let mut out = Vec::new();
    for n in from..=to {
        let Some(header) = r.header(n).map_err(internal)? else { break };
        if !filter.matches_bloom(header.logs_bloom) {
            continue;
        }
        let block = r.block(n).map_err(internal)?.ok_or_else(|| internal("missing block"))?;
        for receipt in receipts_of(r, &block)? {
            for log in receipt.inner.logs() {
                if filter.matches_address(log.address()) && filter.matches_topics(log.topics()) {
                    out.push(to_json(log)?);
                }
            }
        }
    }
    Ok(out)
}
