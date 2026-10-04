//! Boltchain node library: devnet producer, follower, benchmarks, used by the `boltchain` binary.

/// Version with the source commit, e.g. `0.1.0-9ffeafb` (see `build.rs`).
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), "-", env!("BOLT_COMMIT"));

/// Client name and version, e.g. `boltchain/v0.1.0-9ffeafb` (explorer, `web3_clientVersion`).
pub const CLIENT_VERSION: &str =
    concat!("boltchain/v", env!("CARGO_PKG_VERSION"), "-", env!("BOLT_COMMIT"));

pub mod bench;
pub mod certificates;
pub mod checkpoint;
pub mod compute;
pub mod devnet;
pub mod explorer;
pub mod follow;
pub mod gateway;
pub mod keys;
pub mod keystore;
pub mod metrics;
pub mod miner;
pub mod p2p;
pub mod storage;
pub mod swarm;
pub mod validator;
pub mod wallet;
