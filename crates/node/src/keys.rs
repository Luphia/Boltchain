//! Validator key tooling.
//!
//! Becoming a validator is the same for everyone (ADR 0007): nobody is listed in genesis.
//! 1. `boltchain keys new --out validator.key` on the validator machine (the secret stays there).
//! 2. `boltchain keys register-tx --key validator.key --fee-recipient <addr>` prints the
//!    `StakingManager.register` transaction (key, proof of possession); send it from the wallet
//!    that will own the stake, with at least 64 BOLT.

use alloy_primitives::{Address, Bytes, hex};
use alloy_sol_types::SolCall;
use anyhow::{Context, Result, bail};
use bolt_primitives::{
    bls::{self, BlsSecretKey},
    params::{MIN_STAKE_WEI, WEI_PER_BOLT},
};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// On-disk validator key (plain; an encrypted EIP-2335 keystore arrives with M6).
#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeyFile {
    /// Public key (for humans).
    pub bls_public_key: bls::BlsPublicKey,
    /// Secret scalar.
    pub bls_secret_key: String,
}

/// Key commands.
#[derive(Debug, clap::Subcommand)]
pub enum KeysCmd {
    /// Generate a new random BLS validator key.
    New {
        /// Output file (created with 0600 permissions; refuses to overwrite).
        #[arg(long)]
        out: PathBuf,
    },
    /// Write the insecure deterministic development key `index` (dev chains and tests only).
    Dev {
        /// Key index.
        #[arg(long)]
        index: u32,
        /// Output file.
        #[arg(long)]
        out: PathBuf,
    },
    /// Show a key's public key and proof of possession.
    Show {
        /// Key file.
        #[arg(long)]
        key: PathBuf,
    },
    /// Print the `StakingManager.register` transaction for a key (send it with at least 64 BOLT
    /// from the wallet that will own the stake).
    RegisterTx {
        /// Key file.
        #[arg(long)]
        key: PathBuf,
        /// Address that receives rewards and tips.
        #[arg(long)]
        fee_recipient: Address,
    },
    /// Print the `SwarmStorage.setPeer` transaction that publishes the node a validator
    /// serves its history shards from (send it from the validator's owner wallet).
    SetPeerTx {
        /// Validator id.
        #[arg(long)]
        id: u32,
        /// The node's identity key (`<datadir>/node.key`).
        #[arg(long)]
        node_key: PathBuf,
    },
}

fn write_key(path: &Path, key: &BlsSecretKey) -> Result<()> {
    if path.exists() {
        bail!("{} already exists; refusing to overwrite a key", path.display());
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let file = KeyFile {
        bls_public_key: key.public_key(),
        bls_secret_key: hex::encode_prefixed(key.to_bytes()),
    };
    std::fs::write(path, serde_json::to_string_pretty(&file)? + "\n")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Loads a key file.
pub fn load_key(path: &Path) -> Result<BlsSecretKey> {
    let file: KeyFile = serde_json::from_str(
        &std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?,
    )?;
    let bytes = hex::decode(&file.bls_secret_key)?;
    let key = BlsSecretKey::from_bytes(&bytes).context("invalid BLS secret key")?;
    if key.public_key() != file.bls_public_key {
        bail!("key file public key does not match its secret");
    }
    Ok(key)
}

/// Runs a key command.
pub fn run(cmd: KeysCmd) -> Result<()> {
    match cmd {
        KeysCmd::New { out } => {
            let k = BlsSecretKey::random();
            write_key(&out, &k)?;
            println!("BLS public key      {}", k.public_key());
            println!("proof of possession {}", k.proof_of_possession());
            println!("saved to {} (keep it secret; back it up offline)", out.display());
        }
        KeysCmd::Dev { index, out } => {
            let k = bls::dev_key(index);
            write_key(&out, &k)?;
            println!("INSECURE development key {index}: {}", k.public_key());
        }
        KeysCmd::Show { key } => {
            let k = load_key(&key)?;
            println!("BLS public key      {}", k.public_key());
            println!("proof of possession {}", k.proof_of_possession());
        }
        KeysCmd::RegisterTx { key, fee_recipient } => {
            let k = load_key(&key)?;
            println!("{}", serde_json::to_string_pretty(&register_tx(&k, fee_recipient)?)?);
        }
        KeysCmd::SetPeerTx { id, node_key } => {
            let peer = bolt_net::load_or_create_key(&node_key)?.public().to_peer_id();
            println!("{}", serde_json::to_string_pretty(&set_peer_tx(id, &peer))?);
        }
    }
    Ok(())
}

/// The `register` transaction (to, data, minimum value) for `key`.
pub fn register_tx(key: &BlsSecretKey, fee_recipient: Address) -> Result<serde_json::Value> {
    let pk = key.public_key();
    let point = bls::pubkey_point(&pk).context("invalid public key")?;
    let pop = bls::signature_point(&key.proof_of_possession()).context("invalid PoP")?;
    let data = bolt_system::abi::IStakingManager::registerCall {
        pubkey: Bytes::copy_from_slice(pk.as_slice()),
        pubkeyPoint: Bytes::copy_from_slice(&point),
        pop: Bytes::copy_from_slice(&pop),
        feeRecipient: fee_recipient,
    }
    .abi_encode();
    Ok(serde_json::json!({
        "to": bolt_system::addresses::STAKING,
        "data": hex::encode_prefixed(data),
        "minValueWei": MIN_STAKE_WEI.to_string(),
        "minValueBolt": (MIN_STAKE_WEI / WEI_PER_BOLT).to_string(),
    }))
}

/// The `setPeer` transaction (to, data) publishing `peer` for validator `id`.
pub fn set_peer_tx(id: u32, peer: &libp2p::PeerId) -> serde_json::Value {
    let data = bolt_system::abi::ISwarmStorage::setPeerCall {
        id,
        peerId: Bytes::copy_from_slice(&peer.to_bytes()),
    }
    .abi_encode();
    serde_json::json!({
        "to": bolt_system::addresses::SWARM,
        "data": hex::encode_prefixed(data),
        "peerId": peer.to_string(),
    })
}
