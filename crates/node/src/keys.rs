//! Validator key tooling.
//!
//! Workflow for a genesis validator operator:
//! 1. `boltchain keys new --out validator.key` on the validator machine (the secret stays there).
//! 2. `boltchain keys binding-message --key validator.key --address <fee recipient>` and sign the
//!    printed text with the fee-recipient wallet (`personal_sign`, e.g. MetaMask or a hardware wallet).
//! 3. `boltchain keys genesis-entry --key validator.key --name <name> --fee-recipient <addr>
//!    --binding-signature <0x...>` prints the checked JSON entry for `bootstrapValidators`.

use alloy_primitives::{Address, Bytes, hex};
use anyhow::{Context, Result, bail};
use bolt_primitives::{
    bls::{self, BlsSecretKey},
    genesis::BootstrapValidator,
    params::CHAIN_ID,
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
    /// Print the text the fee-recipient address must sign (EIP-191 personal_sign).
    BindingMessage {
        /// Key file.
        #[arg(long)]
        key: PathBuf,
        /// Fee-recipient address.
        #[arg(long)]
        address: Address,
        /// Chain id.
        #[arg(long, default_value_t = CHAIN_ID)]
        chain_id: u64,
    },
    /// Print a verified `bootstrapValidators` entry for a genesis file.
    GenesisEntry {
        /// Key file.
        #[arg(long)]
        key: PathBuf,
        /// Operator name.
        #[arg(long)]
        name: String,
        /// Fee-recipient address.
        #[arg(long)]
        fee_recipient: Address,
        /// personal_sign signature over the binding message (omit only for dev chains).
        #[arg(long)]
        binding_signature: Option<Bytes>,
        /// Chain id.
        #[arg(long, default_value_t = CHAIN_ID)]
        chain_id: u64,
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
        KeysCmd::BindingMessage { key, address, chain_id } => {
            let k = load_key(&key)?;
            println!("{}", bls::binding_message(chain_id, &k.public_key(), &address));
        }
        KeysCmd::GenesisEntry { key, name, fee_recipient, binding_signature, chain_id } => {
            let k = load_key(&key)?;
            let pk = k.public_key();
            if let Some(sig) = &binding_signature
                && !bls::verify_binding(chain_id, &pk, &fee_recipient, sig)
            {
                bail!("binding signature was not made by {fee_recipient} over the binding message");
            }
            let entry = BootstrapValidator {
                name,
                bls_pubkey: pk,
                proof_of_possession: k.proof_of_possession(),
                fee_recipient,
                binding_signature,
            };
            println!("{}", serde_json::to_string_pretty(&entry)?);
        }
    }
    Ok(())
}
