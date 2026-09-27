//! Validator key tooling.
//!
//! Becoming a validator is the same for everyone (ADR 0007): nobody is listed in genesis.
//! 1. `boltchain keys new --out validator.key` on the validator machine (the secret stays there,
//!    in an EIP-2335 keystore encrypted with a password).
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

/// Plain on-disk validator key (development keys and files from before encryption; new keys are
/// EIP-2335 keystores, see [`crate::keystore`]).
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
        /// Output file (created with 0600 permissions; refuses to overwrite). An EIP-2335 keystore
        /// encrypted with a password (asked on the terminal, or `BOLT_PASSWORD_FILE`).
        #[arg(long)]
        out: PathBuf,
        /// Write the secret unencrypted instead (test machines only).
        #[arg(long)]
        plain: bool,
    },
    /// Encrypt an existing plain key file into a new EIP-2335 keystore (the plain file is left
    /// in place: delete it once the new one works).
    Encrypt {
        /// Plain key file.
        #[arg(long)]
        key: PathBuf,
        /// Output keystore.
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

fn write_key(path: &Path, key: &BlsSecretKey, encrypt: bool) -> Result<()> {
    if path.exists() {
        bail!("{} already exists; refusing to overwrite a key", path.display());
    }
    let json = if encrypt {
        let password = crate::keystore::password(&format!("new key {}", path.display()), true)?;
        let ks = crate::keystore::eip2335_encrypt(
            &key.to_bytes(),
            key.public_key().as_slice(),
            &password,
        )?;
        serde_json::to_string_pretty(&ks)?
    } else {
        serde_json::to_string_pretty(&KeyFile {
            bls_public_key: key.public_key(),
            bls_secret_key: hex::encode_prefixed(key.to_bytes()),
        })?
    };
    write_secret(path, &json)
}

/// Writes a secret file with 0600 permissions (creating its directory).
pub fn write_secret(path: &Path, json: &str) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    #[cfg(unix)]
    {
        use std::{io::Write, os::unix::fs::OpenOptionsExt};
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("creating {}", path.display()))?;
        f.write_all(format!("{json}\n").as_bytes())?;
    }
    #[cfg(not(unix))]
    std::fs::write(path, format!("{json}\n"))?;
    Ok(())
}

/// Loads a key file (EIP-2335 keystore or plain).
pub fn load_key(path: &Path) -> Result<BlsSecretKey> {
    Ok(load_key_file(path)?.0)
}

/// Loads a key file; also tells whether it was encrypted.
pub fn load_key_file(path: &Path) -> Result<(BlsSecretKey, bool)> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let v: serde_json::Value = serde_json::from_str(&text)?;
    if crate::keystore::is_encrypted(&v) {
        let ks: crate::keystore::Eip2335 = serde_json::from_value(v)?;
        let key = crate::keystore::unlock(&format!("validator key {}", path.display()), |p| {
            let bytes = crate::keystore::eip2335_decrypt(&ks, p)?;
            BlsSecretKey::from_bytes(&bytes).context("invalid BLS secret key")
        })?;
        if hex::encode(key.public_key().as_slice()) != ks.pubkey.trim_start_matches("0x") {
            bail!("keystore public key does not match its secret");
        }
        return Ok((key, true));
    }
    let file: KeyFile = serde_json::from_value(v)?;
    let bytes = hex::decode(&file.bls_secret_key)?;
    let key = BlsSecretKey::from_bytes(&bytes).context("invalid BLS secret key")?;
    if key.public_key() != file.bls_public_key {
        bail!("key file public key does not match its secret");
    }
    Ok((key, false))
}

/// Runs a key command.
pub fn run(cmd: KeysCmd) -> Result<()> {
    match cmd {
        KeysCmd::New { out, plain } => {
            let k = BlsSecretKey::random();
            write_key(&out, &k, !plain)?;
            println!("BLS public key      {}", k.public_key());
            println!("proof of possession {}", k.proof_of_possession());
            println!("saved to {} (keep it secret; back it up offline)", out.display());
        }
        KeysCmd::Dev { index, out } => {
            let k = bls::dev_key(index);
            write_key(&out, &k, false)?;
            println!("INSECURE development key {index}: {}", k.public_key());
        }
        KeysCmd::Encrypt { key, out } => {
            let (k, encrypted) = load_key_file(&key)?;
            if encrypted {
                bail!("{} is already encrypted", key.display());
            }
            write_key(&out, &k, true)?;
            println!("BLS public key {}", k.public_key());
            println!(
                "encrypted to {}; delete {} once the node starts with it",
                out.display(),
                key.display()
            );
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
