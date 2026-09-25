//! Optional address index for the block explorer (ADR 0010): for every address, the transactions
//! that involve it (sender, recipient, created contract, ERC-20/721 `Transfer` party), newest
//! first. Off unless enabled; kept in step with the chain by `put_block` and
//! `remove_head_block`, and filled for older blocks by [`Tx::index_blocks`].

use crate::{
    Result, StoredBlock, Tx,
    db::{num_key, t},
};
use alloy_consensus::{ReceiptEnvelope, Transaction as _};
use alloy_primitives::{Address, B256, b256};
use libmdbx::{RW, TransactionKind};

/// META: set when the index is enabled (value: the lowest block indexed so far, 8 bytes BE).
pub(crate) const META_ADDR_INDEX: &[u8] = b"addr_index_from";

/// `Transfer(address,address,uint256)` (ERC-20 and ERC-721).
pub const TRANSFER_TOPIC: B256 =
    b256!("ddf252ad1be2c89b69c2b068fc378daa952ba7f163c4a11628f55a4df523b3ef");

/// How an address is involved in a transaction (bit flags).
pub mod role {
    /// Sent it.
    pub const FROM: u8 = 1;
    /// Its recipient.
    pub const TO: u8 = 2;
    /// Contract it created.
    pub const CREATED: u8 = 4;
    /// Sender or recipient of a token `Transfer` it emitted.
    pub const TOKEN: u8 = 8;
    /// Contract that emitted a token `Transfer`.
    pub const TOKEN_CONTRACT: u8 = 16;
}

/// One indexed entry: block, transaction index, roles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AddressTx {
    /// Block number.
    pub block: u64,
    /// Index in the block.
    pub index: u32,
    /// [`role`] flags.
    pub roles: u8,
}

fn key(addr: &Address, block: u64, index: u32) -> [u8; 32] {
    let mut k = [0u8; 32];
    k[..20].copy_from_slice(addr.as_slice());
    k[20..28].copy_from_slice(&num_key(block));
    k[28..].copy_from_slice(&index.to_be_bytes());
    k
}

/// The (address, roles) pairs of transaction `i` of `block`.
fn entries(block: &StoredBlock, receipts: &[ReceiptEnvelope], i: usize) -> Vec<(Address, u8)> {
    let tx = &block.transactions[i];
    let mut out: Vec<(Address, u8)> = Vec::new();
    let mut add = |a: Address, r: u8| match out.iter_mut().find(|(x, _)| *x == a) {
        Some((_, f)) => *f |= r,
        None => out.push((a, r)),
    };
    if let Some(s) = block.senders.get(i) {
        add(*s, role::FROM);
        if tx.kind().is_create() {
            add(s.create(tx.nonce()), role::CREATED);
        }
    }
    if let Some(to) = tx.to() {
        add(to, role::TO);
    }
    if let Some(r) = receipts.get(i) {
        for log in r.logs() {
            let topics = log.topics();
            if topics.len() >= 3 && topics[0] == TRANSFER_TOPIC {
                add(log.address, role::TOKEN_CONTRACT);
                add(Address::from_word(topics[1]), role::TOKEN);
                add(Address::from_word(topics[2]), role::TOKEN);
            }
        }
    }
    out
}

impl<K: TransactionKind> Tx<'_, K> {
    /// Whether the address index is enabled; if so, the lowest block it covers.
    pub fn address_index_from(&self) -> Result<Option<u64>> {
        Ok(self
            .get_raw(t::META, META_ADDR_INDEX)?
            .and_then(|v| v.try_into().ok().map(u64::from_be_bytes)))
    }

    /// Transactions involving `addr`, newest first, strictly older than `before` (block, index)
    /// when given; at most `limit`.
    pub fn address_txs(
        &self,
        addr: &Address,
        before: Option<(u64, u32)>,
        limit: usize,
    ) -> Result<Vec<AddressTx>> {
        let tbl = self.table(t::ADDR_TX)?;
        let mut c = self.txn.cursor(&tbl)?;
        let (b, i) = before.unwrap_or((u64::MAX, u32::MAX));
        let start = key(addr, b, i);
        // Position on the first key >= start, then walk backwards.
        let mut item = match c.set_range::<Vec<u8>, Vec<u8>>(&start)? {
            Some(_) => c.prev::<Vec<u8>, Vec<u8>>()?,
            None => c.last::<Vec<u8>, Vec<u8>>()?,
        };
        let mut out = Vec::new();
        while let Some((k, v)) = item {
            if out.len() >= limit || k.len() != 32 || &k[..20] != addr.as_slice() {
                break;
            }
            if k[20..] < start[20..] {
                out.push(AddressTx {
                    block: u64::from_be_bytes(k[20..28].try_into().expect("8 bytes")),
                    index: u32::from_be_bytes(k[28..32].try_into().expect("4 bytes")),
                    roles: v.first().copied().unwrap_or(0),
                });
            }
            item = c.prev()?;
        }
        Ok(out)
    }
}

impl Tx<'_, RW> {
    /// Turns the address index on; blocks from now on are indexed as they are stored. Older ones
    /// are added by [`Tx::index_blocks`]. Returns the lowest block covered.
    pub fn enable_address_index(&self) -> Result<u64> {
        if let Some(from) = self.address_index_from()? {
            return Ok(from);
        }
        let next = self.head()?.map(|h| h + 1).unwrap_or(0);
        self.put_raw(t::META, META_ADDR_INDEX, &num_key(next))?;
        Ok(next)
    }

    pub(crate) fn index_block(
        &self,
        block: &StoredBlock,
        receipts: &[ReceiptEnvelope],
    ) -> Result<()> {
        for i in 0..block.transactions.len() {
            for (a, roles) in entries(block, receipts, i) {
                let k = key(&a, block.header.number, i as u32);
                let old =
                    self.get_raw(t::ADDR_TX, &k)?.and_then(|v| v.first().copied()).unwrap_or(0);
                self.put_raw(t::ADDR_TX, &k, &[old | roles])?;
            }
        }
        Ok(())
    }

    pub(crate) fn unindex_block(
        &self,
        block: &StoredBlock,
        receipts: &[ReceiptEnvelope],
    ) -> Result<()> {
        for i in 0..block.transactions.len() {
            for (a, _) in entries(block, receipts, i) {
                self.del_raw(t::ADDR_TX, &key(&a, block.header.number, i as u32))?;
            }
        }
        Ok(())
    }

    /// Indexes up to `max` stored blocks below the index's lowest block, newest first (so recent
    /// history is searchable soonest). Blocks without a body (pruned, or before a snapshot base)
    /// end the backfill. Returns how many blocks were indexed; 0 means done.
    pub fn index_blocks(&self, max: u64) -> Result<u64> {
        let Some(mut from) = self.address_index_from()? else { return Ok(0) };
        let floor = self.base()?.max(1);
        let mut n = 0;
        while n < max && from > floor {
            let number = from - 1;
            let Some(block) = self.block(number)? else { break };
            if !self.has_body(number)? && !block.transactions.is_empty() {
                break;
            }
            let receipts = self.receipts(number)?.unwrap_or_default();
            self.index_block(&block, &receipts)?;
            from = number;
            n += 1;
        }
        if n > 0 {
            self.put_raw(t::META, META_ADDR_INDEX, &num_key(from))?;
        }
        Ok(n)
    }
}
