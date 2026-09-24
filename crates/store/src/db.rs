//! libmdbx environment, tables and key/value encodings.

use crate::trie::{NodeSource, NodeWrites};
use alloy_consensus::{Header, ReceiptEnvelope, TxEnvelope};
use alloy_eips::eip2718::{Decodable2718, Encodable2718};
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_rlp::{Decodable, Encodable};
use alloy_trie::{EMPTY_ROOT_HASH, KECCAK_EMPTY};
use libmdbx::{
    Database, DatabaseOptions, Mode, NoWriteMap, RO, RW, ReadWriteOptions, SyncMode, Table,
    TableFlags, Transaction, TransactionKind, WriteFlags,
};
use std::path::Path;

/// All tables. Keys and values are raw bytes; encodings are documented per table.
pub(crate) mod t {
    /// addr(20) -> Account (104 bytes: nonce u64 BE, balance, code_hash, storage_root)
    pub const ACCOUNTS: &str = "accounts";
    /// addr(20) ++ slot(32) -> value(32)
    pub const STORAGE: &str = "storage";
    /// code_hash(32) -> bytecode
    pub const CODES: &str = "codes";
    /// nibble path -> account-trie node RLP
    pub const TRIE_ACC: &str = "trie_acc";
    /// keccak(addr)(32) ++ nibble path -> storage-trie node RLP
    pub const TRIE_STO: &str = "trie_sto";
    /// number(8 BE) -> header RLP
    pub const HEADERS: &str = "headers";
    /// hash(32) -> number(8 BE)
    pub const HASH_NUM: &str = "hash_num";
    /// number(8 BE) -> RLP list of EIP-2718 transactions
    pub const BODIES: &str = "bodies";
    /// number(8 BE) -> concatenated 20-byte senders
    pub const SENDERS: &str = "senders";
    /// number(8 BE) -> RLP list of EIP-2718 receipts
    pub const RECEIPTS: &str = "receipts";
    /// tx hash(32) -> number(8 BE) ++ index(4 BE)
    pub const TX_INDEX: &str = "tx_index";
    /// addr(20) ++ number(8 BE) -> account info (72 bytes) before block `number`, empty = absent
    pub const ACC_HIST: &str = "acc_hist";
    /// addr(20) ++ slot(32) ++ number(8 BE) -> value(32) before block `number`
    pub const STO_HIST: &str = "sto_hist";
    /// number(8 BE) ++ key of acc_hist/sto_hist entry (without the number) -> ()
    pub const HIST_KEYS: &str = "hist_keys";
    /// name -> value
    pub const META: &str = "meta";

    pub const ALL: &[&str] = &[
        ACCOUNTS, STORAGE, CODES, TRIE_ACC, TRIE_STO, HEADERS, HASH_NUM, BODIES, SENDERS, RECEIPTS,
        TX_INDEX, ACC_HIST, STO_HIST, HIST_KEYS, META,
    ];
}

const META_HEAD: &[u8] = b"head";

/// Storage error.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// libmdbx failed.
    #[error("mdbx: {0}")]
    Mdbx(#[from] libmdbx::Error),
    /// A stored value could not be decoded.
    #[error("corrupt value in table {0}")]
    Corrupt(&'static str),
    /// The trie is inconsistent.
    #[error("trie: {0}")]
    Trie(String),
}

/// Result alias.
pub type Result<T, E = StoreError> = std::result::Result<T, E>;

/// Flat account record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Account {
    /// Nonce.
    pub nonce: u64,
    /// Balance in wei.
    pub balance: U256,
    /// keccak256 of the code, `KECCAK_EMPTY` for none.
    pub code_hash: B256,
    /// Root of the account's storage trie.
    pub storage_root: B256,
}

impl Default for Account {
    fn default() -> Self {
        Self {
            nonce: 0,
            balance: U256::ZERO,
            code_hash: KECCAK_EMPTY,
            storage_root: EMPTY_ROOT_HASH,
        }
    }
}

impl Account {
    pub(crate) fn encode(&self) -> [u8; 104] {
        let mut out = [0u8; 104];
        out[..8].copy_from_slice(&self.nonce.to_be_bytes());
        out[8..40].copy_from_slice(&self.balance.to_be_bytes::<32>());
        out[40..72].copy_from_slice(self.code_hash.as_slice());
        out[72..].copy_from_slice(self.storage_root.as_slice());
        out
    }

    pub(crate) fn decode(b: &[u8]) -> Option<Self> {
        if b.len() != 104 {
            return None;
        }
        Some(Self {
            nonce: u64::from_be_bytes(b[..8].try_into().ok()?),
            balance: U256::from_be_slice(&b[8..40]),
            code_hash: B256::from_slice(&b[40..72]),
            storage_root: B256::from_slice(&b[72..]),
        })
    }

    /// Trie leaf value: RLP of `[nonce, balance, storage_root, code_hash]`.
    pub fn trie_value(&self) -> Vec<u8> {
        alloy_rlp::encode(alloy_trie::TrieAccount {
            nonce: self.nonce,
            balance: self.balance,
            storage_root: self.storage_root,
            code_hash: self.code_hash,
        })
    }
}

/// History encoding of account info (no storage root): 72 bytes, or empty for "did not exist".
pub(crate) fn encode_hist_info(info: Option<(u64, U256, B256)>) -> Vec<u8> {
    match info {
        None => Vec::new(),
        Some((nonce, balance, code_hash)) => {
            let mut out = Vec::with_capacity(72);
            out.extend_from_slice(&nonce.to_be_bytes());
            out.extend_from_slice(&balance.to_be_bytes::<32>());
            out.extend_from_slice(code_hash.as_slice());
            out
        }
    }
}

pub(crate) fn decode_hist_info(b: &[u8]) -> Option<Option<(u64, U256, B256)>> {
    match b.len() {
        0 => Some(None),
        72 => Some(Some((
            u64::from_be_bytes(b[..8].try_into().ok()?),
            U256::from_be_slice(&b[8..40]),
            B256::from_slice(&b[40..72]),
        ))),
        _ => None,
    }
}

pub(crate) fn num_key(n: u64) -> [u8; 8] {
    n.to_be_bytes()
}

pub(crate) fn storage_key(addr: &Address, slot: &B256) -> [u8; 52] {
    let mut k = [0u8; 52];
    k[..20].copy_from_slice(addr.as_slice());
    k[20..].copy_from_slice(slot.as_slice());
    k
}

/// A stored block: header plus body with recovered senders.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredBlock {
    /// Header.
    pub header: Header,
    /// Transactions.
    pub transactions: Vec<TxEnvelope>,
    /// Sender of each transaction.
    pub senders: Vec<Address>,
}

/// The node's database.
#[derive(Debug)]
pub struct Store {
    env: Database<NoWriteMap>,
}

impl Store {
    /// Opens (creating if needed) the database in directory `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        std::fs::create_dir_all(path.as_ref()).map_err(|_| StoreError::Corrupt("datadir"))?;
        let env = Database::<NoWriteMap>::open_with_options(
            path,
            DatabaseOptions {
                max_tables: Some(t::ALL.len() as u64 + 4),
                mode: Mode::ReadWrite(ReadWriteOptions {
                    sync_mode: SyncMode::Durable,
                    min_size: None,
                    // 1 TiB address space; the file itself grows in 256 MiB steps.
                    max_size: Some(1 << 40),
                    growth_step: Some(256 << 20),
                    shrink_threshold: None,
                }),
                ..Default::default()
            },
        )?;
        {
            let txn = env.begin_rw_txn()?;
            for name in t::ALL {
                txn.create_table(Some(name), TableFlags::default())?;
            }
            txn.commit()?;
        }
        Ok(Self { env })
    }

    /// Starts a read-only snapshot.
    pub fn reader(&self) -> Result<Tx<'_, RO>> {
        Ok(Tx { txn: self.env.begin_ro_txn()? })
    }

    /// Starts a read-write transaction. Only one can be open at a time.
    pub fn writer(&self) -> Result<Tx<'_, RW>> {
        Ok(Tx { txn: self.env.begin_rw_txn()? })
    }
}

/// A database transaction. Read methods work on both read-only and read-write transactions.
#[derive(Debug)]
pub struct Tx<'e, K: TransactionKind> {
    pub(crate) txn: Transaction<'e, K, NoWriteMap>,
}

impl<'e, K: TransactionKind> Tx<'e, K> {
    pub(crate) fn table(&self, name: &str) -> Result<Table<'_>> {
        Ok(self.txn.open_table(Some(name))?)
    }

    pub(crate) fn get_raw(&self, table: &str, key: &[u8]) -> Result<Option<Vec<u8>>> {
        let tbl = self.table(table)?;
        Ok(self.txn.get::<Vec<u8>>(&tbl, key)?)
    }

    /// Latest persisted block number.
    pub fn head(&self) -> Result<Option<u64>> {
        Ok(self
            .get_raw(t::META, META_HEAD)?
            .map(|v| u64::from_be_bytes(v.try_into().unwrap_or_default())))
    }

    /// Flat account.
    pub fn account(&self, addr: &Address) -> Result<Option<Account>> {
        self.get_raw(t::ACCOUNTS, addr.as_slice())?
            .map(|v| Account::decode(&v).ok_or(StoreError::Corrupt(t::ACCOUNTS)))
            .transpose()
    }

    /// Flat storage slot (zero if unset).
    pub fn storage(&self, addr: &Address, slot: &B256) -> Result<U256> {
        Ok(self
            .get_raw(t::STORAGE, &storage_key(addr, slot))?
            .map(|v| U256::from_be_slice(&v))
            .unwrap_or_default())
    }

    /// Bytecode by hash.
    pub fn code(&self, hash: &B256) -> Result<Option<Bytes>> {
        Ok(self.get_raw(t::CODES, hash.as_slice())?.map(Bytes::from))
    }

    /// Header by number.
    pub fn header(&self, number: u64) -> Result<Option<Header>> {
        self.get_raw(t::HEADERS, &num_key(number))?
            .map(|v| Header::decode(&mut v.as_slice()).map_err(|_| StoreError::Corrupt(t::HEADERS)))
            .transpose()
    }

    /// Block number by hash.
    pub fn block_number(&self, hash: &B256) -> Result<Option<u64>> {
        Ok(self
            .get_raw(t::HASH_NUM, hash.as_slice())?
            .and_then(|v| v.try_into().ok().map(u64::from_be_bytes)))
    }

    /// Block hash by number.
    pub fn block_hash(&self, number: u64) -> Result<Option<B256>> {
        Ok(self.header(number)?.map(|h| h.hash_slow()))
    }

    /// Full block by number.
    pub fn block(&self, number: u64) -> Result<Option<StoredBlock>> {
        let Some(header) = self.header(number)? else { return Ok(None) };
        let raw = self.get_raw(t::BODIES, &num_key(number))?.unwrap_or_default();
        let items: Vec<Bytes> = if raw.is_empty() {
            Vec::new()
        } else {
            Decodable::decode(&mut raw.as_slice()).map_err(|_| StoreError::Corrupt(t::BODIES))?
        };
        let transactions = items
            .iter()
            .map(|b| TxEnvelope::decode_2718(&mut b.as_ref()))
            .collect::<Result<Vec<_>, _>>()
            .map_err(|_| StoreError::Corrupt(t::BODIES))?;
        let senders = self
            .get_raw(t::SENDERS, &num_key(number))?
            .unwrap_or_default()
            .chunks_exact(20)
            .map(Address::from_slice)
            .collect();
        Ok(Some(StoredBlock { header, transactions, senders }))
    }

    /// Receipts of a block.
    pub fn receipts(&self, number: u64) -> Result<Option<Vec<ReceiptEnvelope>>> {
        let Some(raw) = self.get_raw(t::RECEIPTS, &num_key(number))? else { return Ok(None) };
        let items: Vec<Bytes> =
            Decodable::decode(&mut raw.as_slice()).map_err(|_| StoreError::Corrupt(t::RECEIPTS))?;
        items
            .iter()
            .map(|b| ReceiptEnvelope::decode_2718(&mut b.as_ref()))
            .collect::<Result<Vec<_>, _>>()
            .map(Some)
            .map_err(|_| StoreError::Corrupt(t::RECEIPTS))
    }

    /// (block number, index) of a transaction.
    pub fn tx_location(&self, hash: &B256) -> Result<Option<(u64, u32)>> {
        Ok(self.get_raw(t::TX_INDEX, hash.as_slice())?.and_then(|v| {
            (v.len() == 12).then(|| {
                (
                    u64::from_be_bytes(v[..8].try_into().unwrap_or_default()),
                    u32::from_be_bytes(v[8..].try_into().unwrap_or_default()),
                )
            })
        }))
    }

    /// Account info as of the end of block `number`, using recent history.
    /// Returns `None` in the outer option if `number` is older than the retained history.
    pub fn account_at(&self, addr: &Address, number: u64) -> Result<Option<Option<Account>>> {
        if !self.history_covers(number)? {
            return Ok(None);
        }
        let mut cursor = self.txn.cursor(&self.table(t::ACC_HIST)?)?;
        let mut seek = addr.to_vec();
        seek.extend_from_slice(&num_key(number + 1));
        if let Some((k, v)) = cursor.set_range::<Vec<u8>, Vec<u8>>(&seek)?
            && k.starts_with(addr.as_slice())
        {
            let prior = decode_hist_info(&v).ok_or(StoreError::Corrupt(t::ACC_HIST))?;
            return Ok(Some(prior.map(|(nonce, balance, code_hash)| Account {
                nonce,
                balance,
                code_hash,
                storage_root: EMPTY_ROOT_HASH, // not tracked historically
            })));
        }
        Ok(Some(self.account(addr)?))
    }

    /// Storage value as of the end of block `number`, using recent history.
    pub fn storage_at(&self, addr: &Address, slot: &B256, number: u64) -> Result<Option<U256>> {
        if !self.history_covers(number)? {
            return Ok(None);
        }
        let mut cursor = self.txn.cursor(&self.table(t::STO_HIST)?)?;
        let prefix = storage_key(addr, slot);
        let mut seek = prefix.to_vec();
        seek.extend_from_slice(&num_key(number + 1));
        if let Some((k, v)) = cursor.set_range::<Vec<u8>, Vec<u8>>(&seek)?
            && k.starts_with(&prefix)
        {
            return Ok(Some(U256::from_be_slice(&v)));
        }
        Ok(Some(self.storage(addr, slot)?))
    }

    /// Whether state as of block `number` can be reconstructed.
    pub fn history_covers(&self, number: u64) -> Result<bool> {
        let head = self.head()?.unwrap_or(0);
        Ok(number <= head && head - number <= crate::HISTORY_BLOCKS)
    }
}

impl<'e> Tx<'e, RW> {
    pub(crate) fn put_raw(&self, table: &str, key: &[u8], value: &[u8]) -> Result<()> {
        let tbl = self.table(table)?;
        Ok(self.txn.put(&tbl, key, value, WriteFlags::UPSERT)?)
    }

    pub(crate) fn del_raw(&self, table: &str, key: &[u8]) -> Result<()> {
        let tbl = self.table(table)?;
        self.txn.del(&tbl, key, None)?;
        Ok(())
    }

    /// Deletes every key starting with `prefix`.
    pub(crate) fn del_prefix(&self, table: &str, prefix: &[u8]) -> Result<()> {
        let tbl = self.table(table)?;
        let mut cursor = self.txn.cursor(&tbl)?;
        let mut keys = Vec::new();
        let mut item = cursor.set_range::<Vec<u8>, ()>(prefix)?;
        while let Some((k, ())) = item {
            if !k.starts_with(prefix) {
                break;
            }
            keys.push(k);
            item = cursor.next::<Vec<u8>, ()>()?;
        }
        drop(cursor);
        for k in keys {
            self.txn.del(&tbl, &k, None)?;
        }
        Ok(())
    }

    pub(crate) fn apply_node_writes(
        &self,
        table: &str,
        prefix: &[u8],
        writes: NodeWrites,
    ) -> Result<()> {
        for (path, w) in writes {
            let key = [prefix, &path[..]].concat();
            match w {
                Some(rlp) => self.put_raw(table, &key, &rlp)?,
                None => self.del_raw(table, &key)?,
            }
        }
        Ok(())
    }

    /// Writes a block, its senders and receipts, indexes its transactions and moves the head.
    pub fn put_block(&self, block: &StoredBlock, receipts: &[ReceiptEnvelope]) -> Result<B256> {
        let number = block.header.number;
        let hash = block.header.hash_slow();
        let mut buf = Vec::new();
        block.header.encode(&mut buf);
        self.put_raw(t::HEADERS, &num_key(number), &buf)?;
        self.put_raw(t::HASH_NUM, hash.as_slice(), &num_key(number))?;

        let txs: Vec<Bytes> =
            block.transactions.iter().map(|tx| tx.encoded_2718().into()).collect();
        self.put_raw(t::BODIES, &num_key(number), &alloy_rlp::encode(&txs))?;
        let senders: Vec<u8> = block.senders.iter().flat_map(|a| a.0.0).collect();
        self.put_raw(t::SENDERS, &num_key(number), &senders)?;
        let rs: Vec<Bytes> = receipts.iter().map(|r| r.encoded_2718().into()).collect();
        self.put_raw(t::RECEIPTS, &num_key(number), &alloy_rlp::encode(&rs))?;

        for (i, tx) in block.transactions.iter().enumerate() {
            let mut loc = num_key(number).to_vec();
            loc.extend_from_slice(&(i as u32).to_be_bytes());
            self.put_raw(t::TX_INDEX, tx.tx_hash().as_slice(), &loc)?;
        }
        self.put_raw(t::META, META_HEAD, &num_key(number))?;
        Ok(hash)
    }

    /// Commits the transaction.
    pub fn commit(self) -> Result<()> {
        self.txn.commit()?;
        Ok(())
    }
}

/// Account-trie nodes of a transaction.
#[derive(Debug)]
pub struct AccountNodes<'a, 'e, K: TransactionKind>(pub(crate) &'a Tx<'e, K>);

impl<K: TransactionKind> NodeSource for AccountNodes<'_, '_, K> {
    type Error = StoreError;
    fn node(&self, path: &[u8]) -> Result<Option<Vec<u8>>> {
        self.0.get_raw(t::TRIE_ACC, path)
    }
}

/// Storage-trie nodes of one account.
#[derive(Debug)]
pub struct StorageNodes<'a, 'e, K: TransactionKind> {
    pub(crate) tx: &'a Tx<'e, K>,
    pub(crate) hashed: B256,
}

impl<K: TransactionKind> NodeSource for StorageNodes<'_, '_, K> {
    type Error = StoreError;
    fn node(&self, path: &[u8]) -> Result<Option<Vec<u8>>> {
        self.tx.get_raw(t::TRIE_STO, &[self.hashed.as_slice(), path].concat())
    }
}

/// keccak256 of an address, the account's key in the state trie.
pub fn hashed_address(addr: &Address) -> B256 {
    keccak256(addr)
}
