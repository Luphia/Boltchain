//! `boltchain storage`: SwarmStorage deals from the command line (ADR 0014). Files are encrypted
//! with bolt-vault for the wallet's own vault key (derived from the account key), listed in a
//! deal index, handed to a node started with `--rpc-storage`, and paid for with `createDeal`.

use crate::wallet::{format_bolt, load_wallet, parse_bolt, rpc, send_tx};
use alloy_primitives::{Address, B256, Bytes, U256, hex, keccak256};
use alloy_signer_local::PrivateKeySigner;
use alloy_sol_types::SolCall;
use anyhow::{Context, Result, bail};
use bolt_ipld::{Cid, deal::DealIndex};
use bolt_system::{abi::ISwarmStorage, addresses::SWARM};
use serde_json::{Value, json};
use std::path::PathBuf;

const RPC: &str = "http://127.0.0.1:8545";
/// Bytes of blocks per `bolt_hostBlocks` call.
const HOST_BATCH: usize = 4 << 20;

/// Storage commands.
#[derive(Debug, clap::Subcommand)]
pub enum StorageCmd {
    /// Encrypt a file for yourself, hand its blocks to your node (started with `--rpc-storage`)
    /// and pay providers to keep it. Keep the node online until the providers have fetched it
    /// (about one epoch).
    Put {
        /// Account key file (pays; its vault key can read the file).
        #[arg(long)]
        wallet: PathBuf,
        /// File to store.
        file: PathBuf,
        /// Copies (1–16).
        #[arg(long, default_value_t = 3)]
        replicas: u8,
        /// Epochs to keep it (from the current one).
        #[arg(long)]
        epochs: u64,
        /// Price in BOLT per GiB per epoch (providers with a higher minimum are not chosen).
        #[arg(long)]
        price: String,
        /// JSON-RPC endpoint of your node.
        #[arg(long, default_value = RPC)]
        rpc: String,
    },
    /// Fetch a deal's file and decrypt it with your wallet's vault key.
    Get {
        /// Account key file.
        #[arg(long)]
        wallet: PathBuf,
        /// Deal id.
        deal: u64,
        /// Output file.
        #[arg(long)]
        out: PathBuf,
        /// JSON-RPC endpoint of a node started with `--rpc-storage`.
        #[arg(long, default_value = RPC)]
        rpc: String,
    },
    /// Offer user storage from a provider id you control (a validator you own, or a storage
    /// provider registered with `wallet storage-register`).
    Offer {
        /// Account key file (the provider's account).
        #[arg(long)]
        wallet: PathBuf,
        /// Provider id.
        #[arg(long)]
        id: u32,
        /// Capacity in MiB.
        #[arg(long)]
        capacity: u64,
        /// Minimum price in BOLT per GiB per epoch.
        #[arg(long)]
        price: String,
        /// Stop taking new deals instead.
        #[arg(long)]
        close: bool,
        /// JSON-RPC endpoint.
        #[arg(long, default_value = RPC)]
        rpc: String,
    },
    /// Show a deal and its copies.
    Deal {
        /// Deal id.
        deal: u64,
        /// JSON-RPC endpoint.
        #[arg(long, default_value = RPC)]
        rpc: String,
    },
    /// Pay a deal's copies for their finished epochs (anyone may do it), or close it after its
    /// end (paying and refunding the rest).
    Settle {
        /// Account key file (pays the gas).
        #[arg(long)]
        wallet: PathBuf,
        /// Deal id.
        deal: u64,
        /// JSON-RPC endpoint.
        #[arg(long, default_value = RPC)]
        rpc: String,
    },
    /// Keep a deal longer (paying now).
    Extend {
        /// Account key file (the deal's owner).
        #[arg(long)]
        wallet: PathBuf,
        /// Deal id.
        deal: u64,
        /// Extra epochs.
        #[arg(long)]
        epochs: u64,
        /// JSON-RPC endpoint.
        #[arg(long, default_value = RPC)]
        rpc: String,
    },
    /// End a deal after the current epoch; the rest is refunded when it is closed.
    Cancel {
        /// Account key file (the deal's owner).
        #[arg(long)]
        wallet: PathBuf,
        /// Deal id.
        deal: u64,
        /// JSON-RPC endpoint.
        #[arg(long, default_value = RPC)]
        rpc: String,
    },
    /// Withdraw your SwarmStorage balance (payments and refunds).
    Withdraw {
        /// Account key file.
        #[arg(long)]
        wallet: PathBuf,
        /// JSON-RPC endpoint.
        #[arg(long, default_value = RPC)]
        rpc: String,
    },
}

/// The account's vault key pair (X25519), derived from its private key.
pub fn vault_key(s: &PrivateKeySigner) -> (bolt_vault::SecretKey, bolt_vault::PublicKey) {
    let ikm = keccak256([b"boltchain/vault-key/v1".as_slice(), s.to_bytes().as_slice()].concat());
    bolt_vault::SecretKey::derive(ikm.as_slice())
}

fn call<C: SolCall>(url: &str, c: C) -> Result<C::Return> {
    let out = rpc(
        url,
        "eth_call",
        json!([{"to": SWARM, "data": hex::encode_prefixed(c.abi_encode())}, "latest"]),
    )?;
    let bytes: Bytes = serde_json::from_value(out)?;
    Ok(C::abi_decode_returns(&bytes)?)
}

fn to_ipld(c: &bolt_vault::Cid) -> Cid {
    Cid::try_from(c.to_bytes().as_slice()).expect("vault CIDs are raw sha2-256 CIDv1")
}

/// Per-copy cost of one epoch: `ceil(price * MiB / 1024)`.
pub fn per_epoch(price: U256, size: u64) -> U256 {
    let mib = U256::from(size.div_ceil(1 << 20));
    (price * mib + U256::from(1023)) / U256::from(1024)
}

/// Runs a storage command.
pub fn run(cmd: StorageCmd) -> Result<()> {
    match cmd {
        StorageCmd::Put { wallet, file, replicas, epochs, price, rpc: url } => {
            let s = load_wallet(&wallet)?;
            let data =
                std::fs::read(&file).with_context(|| format!("reading {}", file.display()))?;
            let name =
                file.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let (_, pk) = vault_key(&s);
            let sealed = bolt_vault::seal(
                &data,
                &name,
                "application/octet-stream",
                &[pk],
                &bolt_vault::Options::default(),
            )?;
            let mut listed: Vec<(Cid, Vec<u8>)> = sealed
                .shards
                .iter()
                .chain([&sealed.manifest, &sealed.envelope])
                .map(|b| (to_ipld(&b.cid), b.data.clone()))
                .collect();
            let sizes: Vec<(Cid, u64)> = listed.iter().map(|(c, b)| (*c, b.len() as u64)).collect();
            let (root, idx, index_blocks) =
                bolt_ipld::deal::deal_index(&sizes, Some(to_ipld(&sealed.envelope.cid)));
            let size = idx.size + index_blocks.iter().map(|(_, b)| b.len() as u64).sum::<u64>();
            listed.extend(index_blocks);
            // Hand the blocks to the node in batches.
            let mut batch: Vec<Value> = Vec::new();
            let mut bytes = 0;
            let n = listed.len();
            for (i, (c, b)) in listed.iter().enumerate() {
                bytes += b.len();
                batch.push(json!([c.to_string(), hex::encode_prefixed(b)]));
                if bytes >= HOST_BATCH || i + 1 == n {
                    rpc(&url, "bolt_hostBlocks", json!([root.to_string(), batch]))?;
                    batch = Vec::new();
                    bytes = 0;
                }
            }
            println!("file sealed: {} blocks, {size} bytes, deal index {root}", idx.count);
            let price = parse_bolt(&price)?;
            let value = per_epoch(price, size) * U256::from(replicas) * U256::from(epochs);
            let data = ISwarmStorage::createDealCall {
                root: Bytes::from(root.to_bytes()),
                blocks: idx.count,
                size,
                replicas,
                epochs,
                price: price.to::<u128>(),
            }
            .abi_encode();
            println!("escrow {} BOLT", format_bolt(value));
            let hash = send_tx(&url, &s, SWARM, value, data.into())?;
            let id = deal_id(&url, hash)?;
            println!("deal {id} created; keep this node online until the providers fetch it");
            println!(
                "read it back with: boltchain storage get --wallet <wallet> {id} --out <file>"
            );
        }
        StorageCmd::Get { wallet, deal, out, rpc: url } => {
            let s = load_wallet(&wallet)?;
            let (sk, _) = vault_key(&s);
            let d = call(&url, ISwarmStorage::dealCall { id: U256::from(deal) })?;
            let root = Cid::try_from(d.root.as_ref()).context("deal root")?;
            let get = |cids: &[Cid]| -> Result<Vec<Vec<u8>>> {
                let mut out = Vec::new();
                for chunk in cids.chunks(64) {
                    let names: Vec<String> = chunk.iter().map(|c| c.to_string()).collect();
                    let v = rpc(&url, "bolt_getBlocks", json!([names, deal]))?;
                    let got: Vec<Bytes> = serde_json::from_value(v)?;
                    out.extend(got.into_iter().map(|b| b.to_vec()));
                }
                Ok(out)
            };
            let idx = DealIndex::decode(&get(&[root])?[0])?;
            let entry = idx.root.context("the deal index names no file")?;
            let envelope = get(&[entry])?.remove(0);
            let env = bolt_vault::parse_envelope(&envelope)?;
            let key = bolt_vault::open_key(&env, &sk)?;
            let mblock = get(&[to_ipld(&env.manifest)])?.remove(0);
            let manifest = bolt_vault::open_manifest(&env, &mblock, &key)?;
            let cids: Vec<Cid> = manifest.shards.iter().map(to_ipld).collect();
            let shards: Vec<Option<Vec<u8>>> = get(&cids)?.into_iter().map(Some).collect();
            let data = bolt_vault::recover(&manifest, &key, &shards)?;
            std::fs::write(&out, &data)?;
            println!("{} ({} bytes) written to {}", manifest.name, data.len(), out.display());
        }
        StorageCmd::Offer { wallet, id, capacity, price, close, rpc: url } => {
            let s = load_wallet(&wallet)?;
            let data = if close {
                ISwarmStorage::closeOfferCall { id }.abi_encode()
            } else {
                let min_price = parse_bolt(&price)?.to::<u128>();
                ISwarmStorage::offerStorageCall { id, capacityMiB: capacity, minPrice: min_price }
                    .abi_encode()
            };
            send_tx(&url, &s, SWARM, U256::ZERO, data.into())?;
            let o = call(&url, ISwarmStorage::offerCall { id })?;
            println!(
                "provider {id}: {} MiB ({} used), minimum {} BOLT/GiB/epoch, {}",
                o.capacityMiB,
                o.usedMiB,
                format_bolt(U256::from(o.minPrice)),
                if o.open { "open" } else { "closed" }
            );
        }
        StorageCmd::Deal { deal, rpc: url } => {
            let d = call(&url, ISwarmStorage::dealCall { id: U256::from(deal) })?;
            if d.owner == Address::ZERO {
                bail!("no deal {deal}");
            }
            let root = Cid::try_from(d.root.as_ref()).map(|c| c.to_string()).unwrap_or_default();
            println!("deal {deal}: owner {}, index {root}", d.owner);
            println!(
                "  {} blocks, {} bytes, {} copies, epochs {}..{}, {} BOLT/copy/epoch, escrow {} BOLT{}",
                d.blocks,
                d.size,
                d.replicas,
                d.startEpoch,
                d.endEpoch,
                format_bolt(U256::from(d.perEpoch)),
                format_bolt(d.escrow),
                if d.closed { ", closed" } else { "" }
            );
            let sl = call(&url, ISwarmStorage::dealSlotsCall { id: U256::from(deal) })?;
            for k in 0..sl.providers.len() {
                println!(
                    "  copy {k}: provider {} since epoch {}, paid through {}{}",
                    sl.providers[k],
                    sl.since[k],
                    sl.paidThrough[k],
                    if sl.open[k] { "" } else { " (dropped)" }
                );
            }
        }
        StorageCmd::Settle { wallet, deal, rpc: url } => {
            let s = load_wallet(&wallet)?;
            let d = call(&url, ISwarmStorage::dealCall { id: U256::from(deal) })?;
            let now = crate::wallet::current_epoch(&url)?;
            if now >= d.endEpoch && !d.closed {
                let data = ISwarmStorage::closeDealCall { id: U256::from(deal) }.abi_encode();
                send_tx(&url, &s, SWARM, U256::ZERO, data.into())?;
                println!("deal {deal} closed");
                return Ok(());
            }
            let sl = call(&url, ISwarmStorage::dealSlotsCall { id: U256::from(deal) })?;
            for k in 0..sl.providers.len() {
                if !sl.open[k] || sl.paidThrough[k] >= now {
                    continue;
                }
                let data = ISwarmStorage::claimCall { id: U256::from(deal), slot: U256::from(k) }
                    .abi_encode();
                match send_tx(&url, &s, SWARM, U256::ZERO, data.into()) {
                    Ok(_) => println!("copy {k} paid"),
                    Err(e) => println!("copy {k}: {e:#}"),
                }
            }
        }
        StorageCmd::Extend { wallet, deal, epochs, rpc: url } => {
            let s = load_wallet(&wallet)?;
            let d = call(&url, ISwarmStorage::dealCall { id: U256::from(deal) })?;
            let value = U256::from(d.perEpoch) * U256::from(d.replicas) * U256::from(epochs);
            let data = ISwarmStorage::extendDealCall { id: U256::from(deal), epochs }.abi_encode();
            send_tx(&url, &s, SWARM, value, data.into())?;
            println!("deal {deal} now ends at epoch {}", d.endEpoch + epochs);
        }
        StorageCmd::Cancel { wallet, deal, rpc: url } => {
            let s = load_wallet(&wallet)?;
            let data = ISwarmStorage::cancelDealCall { id: U256::from(deal) }.abi_encode();
            send_tx(&url, &s, SWARM, U256::ZERO, data.into())?;
            println!("deal {deal} ends after this epoch; close it then with `storage settle`");
        }
        StorageCmd::Withdraw { wallet, rpc: url } => {
            let s = load_wallet(&wallet)?;
            let b = call(&url, ISwarmStorage::balanceOfCall { account: s.address() })?;
            if b.is_zero() {
                println!("nothing to withdraw");
                return Ok(());
            }
            let data = ISwarmStorage::withdrawCall { to: s.address() }.abi_encode();
            send_tx(&url, &s, SWARM, U256::ZERO, data.into())?;
            println!("withdrew {} BOLT", format_bolt(b));
        }
    }
    Ok(())
}

/// The id of the deal created by transaction `hash` (from its `DealCreated` log).
fn deal_id(url: &str, hash: B256) -> Result<U256> {
    let topic = keccak256(
        "DealCreated(uint256,address,bytes,uint64,uint64,uint8,uint64,uint64,uint128)".as_bytes(),
    );
    let r = rpc(url, "eth_getTransactionReceipt", json!([hash]))?;
    for log in r["logs"].as_array().into_iter().flatten() {
        let topics: Vec<B256> = serde_json::from_value(log["topics"].clone()).unwrap_or_default();
        if topics.first() == Some(&topic) && topics.len() > 1 {
            return Ok(U256::from_be_bytes(topics[1].0));
        }
    }
    bail!("no DealCreated log in {hash}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deal_costs_round_up() {
        let bolt = U256::from(10u128.pow(18));
        // 3 MiB at 1 BOLT/GiB: 3/1024 BOLT, rounded up to the wei.
        assert_eq!(
            per_epoch(bolt, 3 << 20),
            (bolt * U256::from(3) + U256::from(1023)) / U256::from(1024)
        );
        // 1 byte bills as 1 MiB.
        assert_eq!(per_epoch(bolt, 1), per_epoch(bolt, 1 << 20));
    }

    #[test]
    fn vault_key_is_stable_per_account() {
        let s = PrivateKeySigner::random();
        assert_eq!(vault_key(&s).1.to_bytes(), vault_key(&s).1.to_bytes());
        assert_ne!(vault_key(&s).1.to_bytes(), vault_key(&PrivateKeySigner::random()).1.to_bytes());
    }
}
