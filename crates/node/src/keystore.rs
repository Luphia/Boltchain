//! Encrypted key files.
//!
//! - Validator BLS keys: EIP-2335 keystores (scrypt, AES-128-CTR, SHA-256 checksum), the format
//!   Ethereum consensus clients use.
//! - Account keys: Web3 Secret Storage v3 (scrypt, AES-128-CTR, Keccak-256 MAC), the format
//!   geth and MetaMask import and export.
//!
//! Both use scrypt with n = 2^18, r = 8, p = 1 (about 256 MiB and a second to open). Passwords
//! come from `BOLT_PASSWORD_FILE` (a file holding the password), `BOLT_PASSWORD`, or a prompt on
//! the terminal, in that order.

use alloy_primitives::{hex, keccak256};
use anyhow::{Context, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;

/// scrypt cost used for new files (log2 n, r, p).
const SCRYPT: (u8, u32, u32) = (18, 8, 1);

type Aes128Ctr = ctr::Ctr128BE<aes::Aes128>;

/// Reads the password for `what` (e.g. "validator key v1.json"); `confirm` asks twice on a
/// terminal (for new files).
pub fn password(what: &str, confirm: bool) -> Result<String> {
    if let Ok(path) = std::env::var("BOLT_PASSWORD_FILE") {
        let s = std::fs::read_to_string(&path).with_context(|| format!("reading {path}"))?;
        return Ok(s.trim_end_matches(['\n', '\r']).to_string());
    }
    if let Ok(s) = std::env::var("BOLT_PASSWORD") {
        return Ok(s);
    }
    use std::io::IsTerminal;
    ensure!(
        std::io::stdin().is_terminal(),
        "{what} is encrypted: set BOLT_PASSWORD_FILE (or BOLT_PASSWORD) or run in a terminal"
    );
    let p = rpassword::prompt_password(format!("Password for {what}: "))?;
    if confirm {
        ensure!(p.chars().count() >= 8, "use a password of at least 8 characters");
        let again = rpassword::prompt_password("Repeat the password: ")?;
        ensure!(p == again, "the passwords differ");
    }
    Ok(p)
}

static LAST: parking_lot::Mutex<Option<String>> = parking_lot::Mutex::new(None);

/// Opens an encrypted file with `open`, reusing the password that opened the previous one (a
/// validator with several keys under one password is asked once).
pub fn unlock<T>(what: &str, open: impl Fn(&str) -> Result<T>) -> Result<T> {
    if let Some(p) = LAST.lock().clone()
        && let Ok(v) = open(&p)
    {
        return Ok(v);
    }
    let p = password(what, false)?;
    let v = open(&p).with_context(|| format!("opening {what}"))?;
    *LAST.lock() = Some(p);
    Ok(v)
}

/// Whether a key file's JSON is encrypted (EIP-2335 or v3).
pub fn is_encrypted(v: &Value) -> bool {
    v.get("crypto").or_else(|| v.get("Crypto")).is_some()
}

fn random<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::fill(&mut b).expect("OS randomness");
    b
}

fn uuid_v4() -> String {
    let mut b: [u8; 16] = random();
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    let h = hex::encode(b);
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

fn scrypt_key(password: &[u8], salt: &[u8], log_n: u8, r: u32, p: u32) -> Result<[u8; 32]> {
    ensure!((10..=20).contains(&log_n), "unsupported scrypt n = 2^{log_n}");
    let params = scrypt::Params::new(log_n, r, p, 32)
        .map_err(|e| anyhow::anyhow!("scrypt parameters: {e}"))?;
    let mut dk = [0u8; 32];
    scrypt::scrypt(password, salt, &params, &mut dk).map_err(|e| anyhow::anyhow!("scrypt: {e}"))?;
    Ok(dk)
}

fn log2_exact(n: u64) -> Result<u8> {
    ensure!(n.is_power_of_two() && n > 1, "scrypt n must be a power of two");
    Ok(n.trailing_zeros() as u8)
}

fn aes_ctr(key: &[u8], iv: &[u8], data: &mut [u8]) -> Result<()> {
    use ctr::cipher::{KeyIvInit, StreamCipher};
    ensure!(key.len() == 16 && iv.len() == 16, "AES-128-CTR needs a 16-byte key and IV");
    Aes128Ctr::new(key.into(), iv.into()).apply_keystream(data);
    Ok(())
}

/// EIP-2335 password processing: NFKD, then drop C0, C1 and Delete control codes.
fn eip2335_password(p: &str) -> Vec<u8> {
    p.nfkd()
        .filter(|c| !matches!(*c as u32, 0x00..=0x1f | 0x7f..=0x9f))
        .collect::<String>()
        .into_bytes()
}

// ---------- EIP-2335 (validator keys) ----------

#[derive(Debug, Serialize, Deserialize)]
struct Module {
    function: String,
    #[serde(default)]
    params: Value,
    message: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct Eip2335Crypto {
    kdf: Module,
    checksum: Module,
    cipher: Module,
}

/// An EIP-2335 keystore.
#[derive(Debug, Serialize, Deserialize)]
pub struct Eip2335 {
    crypto: Eip2335Crypto,
    #[serde(default)]
    description: String,
    /// Public key, hex without 0x.
    pub pubkey: String,
    #[serde(default)]
    path: String,
    uuid: String,
    version: u32,
}

/// Encrypts a 32-byte secret into an EIP-2335 keystore for `pubkey`.
pub fn eip2335_encrypt(secret: &[u8], pubkey: &[u8], password: &str) -> Result<Eip2335> {
    let (log_n, r, p) = SCRYPT;
    eip2335_encrypt_with(secret, pubkey, password, random(), random(), log_n, r, p)
}

#[allow(clippy::too_many_arguments)]
fn eip2335_encrypt_with(
    secret: &[u8],
    pubkey: &[u8],
    password: &str,
    salt: [u8; 32],
    iv: [u8; 16],
    log_n: u8,
    r: u32,
    p: u32,
) -> Result<Eip2335> {
    let dk = scrypt_key(&eip2335_password(password), &salt, log_n, r, p)?;
    let mut ct = secret.to_vec();
    aes_ctr(&dk[..16], &iv, &mut ct)?;
    let checksum = Sha256::digest([&dk[16..], &ct[..]].concat());
    Ok(Eip2335 {
        crypto: Eip2335Crypto {
            kdf: Module {
                function: "scrypt".into(),
                params: json!({ "dklen": 32, "n": 1u64 << log_n, "r": r, "p": p, "salt": hex::encode(salt) }),
                message: String::new(),
            },
            checksum: Module {
                function: "sha256".into(),
                params: json!({}),
                message: hex::encode(checksum),
            },
            cipher: Module {
                function: "aes-128-ctr".into(),
                params: json!({ "iv": hex::encode(iv) }),
                message: hex::encode(ct),
            },
        },
        description: String::new(),
        pubkey: hex::encode(pubkey),
        path: String::new(),
        uuid: uuid_v4(),
        version: 4,
    })
}

fn param_u64(p: &Value, k: &str) -> Result<u64> {
    p.get(k).and_then(Value::as_u64).with_context(|| format!("missing kdf parameter {k}"))
}

fn param_hex(p: &Value, k: &str) -> Result<Vec<u8>> {
    Ok(hex::decode(p.get(k).and_then(Value::as_str).with_context(|| format!("missing {k}"))?)?)
}

/// Decrypts an EIP-2335 keystore (scrypt or PBKDF2-SHA256).
pub fn eip2335_decrypt(ks: &Eip2335, password: &str) -> Result<Vec<u8>> {
    ensure!(ks.version == 4, "unsupported keystore version {}", ks.version);
    let c = &ks.crypto;
    let pw = eip2335_password(password);
    let kp = &c.kdf.params;
    let salt = param_hex(kp, "salt")?;
    ensure!(param_u64(kp, "dklen")? == 32, "unsupported dklen");
    let dk: [u8; 32] = match c.kdf.function.as_str() {
        "scrypt" => scrypt_key(
            &pw,
            &salt,
            log2_exact(param_u64(kp, "n")?)?,
            param_u64(kp, "r")? as u32,
            param_u64(kp, "p")? as u32,
        )?,
        "pbkdf2" => {
            ensure!(
                kp.get("prf").and_then(Value::as_str) == Some("hmac-sha256"),
                "unsupported prf"
            );
            let mut dk = [0u8; 32];
            pbkdf2::pbkdf2_hmac::<Sha256>(&pw, &salt, param_u64(kp, "c")? as u32, &mut dk);
            dk
        }
        f => bail!("unsupported kdf {f}"),
    };
    ensure!(c.checksum.function == "sha256", "unsupported checksum {}", c.checksum.function);
    ensure!(c.cipher.function == "aes-128-ctr", "unsupported cipher {}", c.cipher.function);
    let mut ct = hex::decode(&c.cipher.message)?;
    let sum = Sha256::digest([&dk[16..], &ct[..]].concat());
    ensure!(hex::decode(&c.checksum.message)? == sum[..], "wrong password");
    aes_ctr(&dk[..16], &param_hex(&c.cipher.params, "iv")?, &mut ct)?;
    Ok(ct)
}

// ---------- Web3 Secret Storage v3 (account keys) ----------

/// Encrypts a 32-byte account key into a v3 keystore for `address`.
pub fn v3_encrypt(secret: &[u8], address: &[u8], password: &str) -> Result<Value> {
    let (log_n, r, p) = SCRYPT;
    let (salt, iv): ([u8; 32], [u8; 16]) = (random(), random());
    let dk = scrypt_key(password.as_bytes(), &salt, log_n, r, p)?;
    let mut ct = secret.to_vec();
    aes_ctr(&dk[..16], &iv, &mut ct)?;
    let mac = keccak256([&dk[16..], &ct[..]].concat());
    Ok(json!({
        "address": hex::encode(address),
        "crypto": {
            "cipher": "aes-128-ctr",
            "cipherparams": { "iv": hex::encode(iv) },
            "ciphertext": hex::encode(ct),
            "kdf": "scrypt",
            "kdfparams": { "dklen": 32, "n": 1u64 << log_n, "r": r, "p": p, "salt": hex::encode(salt) },
            "mac": hex::encode(mac),
        },
        "id": uuid_v4(),
        "version": 3,
    }))
}

/// Decrypts a v3 keystore (scrypt or PBKDF2-SHA256) to the 32-byte key.
pub fn v3_decrypt(v: &Value, password: &str) -> Result<Vec<u8>> {
    ensure!(v.get("version").and_then(Value::as_u64) == Some(3), "unsupported keystore version");
    let c = v.get("crypto").or_else(|| v.get("Crypto")).context("missing crypto")?;
    let s = |k: &str| c.get(k).and_then(Value::as_str).with_context(|| format!("missing {k}"));
    ensure!(s("cipher")? == "aes-128-ctr", "unsupported cipher");
    let kp = c.get("kdfparams").context("missing kdfparams")?;
    let salt = param_hex(kp, "salt")?;
    ensure!(param_u64(kp, "dklen")? == 32, "unsupported dklen");
    let dk: [u8; 32] = match s("kdf")? {
        "scrypt" => scrypt_key(
            password.as_bytes(),
            &salt,
            log2_exact(param_u64(kp, "n")?)?,
            param_u64(kp, "r")? as u32,
            param_u64(kp, "p")? as u32,
        )?,
        "pbkdf2" => {
            ensure!(
                kp.get("prf").and_then(Value::as_str) == Some("hmac-sha256"),
                "unsupported prf"
            );
            let mut dk = [0u8; 32];
            pbkdf2::pbkdf2_hmac::<Sha256>(
                password.as_bytes(),
                &salt,
                param_u64(kp, "c")? as u32,
                &mut dk,
            );
            dk
        }
        f => bail!("unsupported kdf {f}"),
    };
    let mut ct = hex::decode(s("ciphertext")?)?;
    ensure!(
        hex::decode(s("mac")?)? == keccak256([&dk[16..], &ct[..]].concat()).as_slice(),
        "wrong password"
    );
    aes_ctr(
        &dk[..16],
        &param_hex(c.get("cipherparams").context("missing cipherparams")?, "iv")?,
        &mut ct,
    )?;
    Ok(ct)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The scrypt test vector from EIP-2335.
    #[test]
    fn eip2335_test_vector() {
        let ks: Eip2335 = serde_json::from_value(json!({
            "crypto": {
                "kdf": { "function": "scrypt", "params": { "dklen": 32, "n": 262144, "p": 1, "r": 8,
                    "salt": "d4e56740f876aef8c010b86a40d5f56745a118d0906a34e69aec8c0db1cb8fa3" }, "message": "" },
                "checksum": { "function": "sha256", "params": {},
                    "message": "d2217fe5f3e9a1e34581ef8a78f7c9928e436d36dacc5e846690a5581e8ea484" },
                "cipher": { "function": "aes-128-ctr", "params": { "iv": "264daa3f303d7259501c93d997d84fe6" },
                    "message": "06ae90d55fe0a6e9c5c3bc5b170827b2e5cce3929ed3f116c2811e6366dfe20f" }
            },
            "description": "This is a test keystore that uses scrypt to secure the secret.",
            "pubkey": "9612d7a727c9d0a22e185a1c768478dfe919cada9266988cb32359c11f2b7b27f4ae4040902382ae2910c15e2b420d07",
            "path": "m/12381/60/3141592653/589793238",
            "uuid": "1d85ae20-35c5-4611-98e8-aa14a633906f",
            "version": 4
        }))
        .unwrap();
        let secret = eip2335_decrypt(&ks, "𝔱𝔢𝔰𝔱𝔭𝔞𝔰𝔰𝔴𝔬𝔯𝔡🔑").unwrap();
        assert_eq!(
            hex::encode(secret),
            "000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f"
        );
        assert!(eip2335_decrypt(&ks, "testpassword").is_err(), "the key emoji is part of it");
        // Encrypting with the vector's salt and IV reproduces it.
        let again = eip2335_encrypt_with(
            &hex::decode("000000000019d6689c085ae165831e934ff763ae46a2a6c172b3f1b60a8ce26f")
                .unwrap(),
            &hex::decode(&ks.pubkey).unwrap(),
            "testpassword🔑",
            hex::decode("d4e56740f876aef8c010b86a40d5f56745a118d0906a34e69aec8c0db1cb8fa3")
                .unwrap()
                .try_into()
                .unwrap(),
            hex::decode("264daa3f303d7259501c93d997d84fe6").unwrap().try_into().unwrap(),
            18,
            8,
            1,
        )
        .unwrap();
        assert_eq!(again.crypto.cipher.message, ks.crypto.cipher.message);
        assert_eq!(again.crypto.checksum.message, ks.crypto.checksum.message);
    }

    /// A v3 keystore written by eth-account (Python), the library MetaMask-compatible tools use.
    /// (The Web3 Secret Storage definition's own vector uses r = 1 with n = 2^18, which the
    /// scrypt crate rejects as outside RFC 7914; geth and MetaMask write r = 8.)
    #[test]
    fn v3_from_eth_account() {
        let v = json!({
            "address": "008AeEda4D805471dF9b2A5B0f38A0C3bCBA786b",
            "crypto": {
                "cipher": "aes-128-ctr",
                "cipherparams": { "iv": "b4eeee20bf699c983b57dd273c6c3be9" },
                "ciphertext": "a196d34a8d2bcd882f2c5a07a0951af772b00c433d2560a15454780f92e32c17",
                "kdf": "scrypt",
                "kdfparams": { "dklen": 32, "n": 262144, "r": 8, "p": 1, "salt": "c834343b0e4b4072ba69994285ee3d10" },
                "mac": "b94eb2235bddfb6f141776e6ad8a40c6c686a7d18ce803e04920b80713e21b38"
            },
            "id": "1d14d02a-a28f-4894-a567-5f8791edc37b",
            "version": 3
        });
        assert_eq!(
            hex::encode(v3_decrypt(&v, "testpassword").unwrap()),
            "7a28b5ba57c53603b0b07b56bba752f7784bf506fa95edc395f5cf6c7514fe9d"
        );
        assert!(v3_decrypt(&v, "wrong").is_err());
    }

    #[test]
    fn round_trips() {
        let secret: [u8; 32] = random();
        let ks = eip2335_encrypt(&secret, &[1; 48], "correct horse").unwrap();
        let back: Eip2335 = serde_json::from_str(&serde_json::to_string(&ks).unwrap()).unwrap();
        assert_eq!(eip2335_decrypt(&back, "correct horse").unwrap(), secret);
        let v = v3_encrypt(&secret, &[2; 20], "correct horse").unwrap();
        assert!(is_encrypted(&v));
        assert_eq!(v3_decrypt(&v, "correct horse").unwrap(), secret);
        assert!(v3_decrypt(&v, "correct horsE").is_err());
    }
}
