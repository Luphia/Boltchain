//! Local storage on libmdbx: flat state, path-based MPT, blocks, receipts, recent state history
//! and (from M2) the IPFS blockstore.
//!
//! Execution reads the flat `accounts`/`storage` tables; the tries are only touched when a block
//! is committed, and only along the paths that block changed.

pub mod db;
pub mod state;
pub mod trie;

pub use db::{Account, Result, Store, StoreError, StoredBlock, Tx};
pub use libmdbx::{RO, RW};
pub use state::{InitAccount, StateView, full_state_root};

/// Blocks of state history kept for historical `eth_call` and friends.
pub const HISTORY_BLOCKS: u64 = 128;

#[cfg(test)]
mod tests;
