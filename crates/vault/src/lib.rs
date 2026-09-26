//! Boltchain Vault (ADR 0011): a file is padded, cut into chunks, each chunk encrypted
//! (XChaCha20-Poly1305), the ciphertext erasure-coded (Reed-Solomon, `k` data + `m` parity
//! shards per stripe) and every shard stored as an IPFS raw block. A manifest listing the shards
//! (and the file name, type and size) is encrypted with the same content key. The content key is
//! wrapped with HPKE (RFC 9180, X25519) for each recipient; only they can read the file.
//!
//! What is public: the envelope (manifest CID, wrapped keys) and the shards, i.e. ciphertext and
//! the padded size (a bucket, not the exact size). Storage nodes and auditors can check shards
//! against their CIDs without any key.
//!
//! Layout of a sealed file:
//! - shards: raw blocks, stripe after stripe (`k + m` per stripe);
//! - manifest: raw block, encrypted JSON ([`Manifest`]);
//! - envelope: raw block, plain JSON ([`Envelope`]); its CID names the file.

use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use hpke::{Deserializable, Kem as _, OpModeR, OpModeS, Serializable};
use rand_core::{OsRng, RngCore, UnwrapErr};
use reed_solomon_erasure::galois_8::ReedSolomon;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[cfg(feature = "wasm")]
pub mod wasm;

/// Format version.
pub const VERSION: u32 = 1;
/// Largest plaintext chunk; a shard is a chunk plus its 16-byte tag (fits any IPFS block limit).
pub const MAX_CHUNK: usize = 256 * 1024;
/// Smallest plaintext chunk.
pub const MIN_CHUNK: usize = 4 * 1024;
/// Authentication tag per chunk.
pub const TAG: usize = 16;
/// Default data shards per stripe.
pub const DEFAULT_K: usize = 4;
/// Default parity shards per stripe (any 2 of 6 may be lost).
pub const DEFAULT_M: usize = 2;

const CHUNK_AAD: &[u8] = b"bolt-vault/v1/chunk";
const MANIFEST_AAD: &[u8] = b"bolt-vault/v1/manifest";
const KEY_INFO: &[u8] = b"bolt-vault/v1/key";

type Kem = hpke::kem::X25519HkdfSha256;
type Kdf = hpke::kdf::HkdfSha256;
type WrapAead = hpke::aead::ChaCha20Poly1305;

/// Errors.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// Parameters out of range.
    #[error("invalid parameters: {0}")]
    Params(String),
    /// None of the wrapped keys opens with this secret key.
    #[error("not a recipient of this file")]
    NotRecipient,
    /// A block does not match its CID, fails authentication, or is malformed.
    #[error("corrupt data: {0}")]
    Corrupt(String),
    /// Too few intact shards in a stripe.
    #[error("stripe {stripe}: {have} intact shards, {need} needed")]
    TooFewShards {
        /// Stripe index.
        stripe: usize,
        /// Intact shards.
        have: usize,
        /// Data shards needed.
        need: usize,
    },
}

/// Result.
pub type Result<T> = std::result::Result<T, Error>;

// ---------------------------------------------------------------------------------------------
// CIDs (CIDv1, raw codec, sha2-256), without pulling in a CID library (the crate also builds for
// the browser).

/// CIDv1 of a raw block (codec 0x55, sha2-256).
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cid(pub [u8; 32]);

impl Cid {
    /// CID of `data`.
    pub fn of(data: &[u8]) -> Self {
        Self(Sha256::digest(data).into())
    }

    /// Binary form: `0x01 0x55 0x12 0x20 ‖ digest`.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut b = vec![0x01, 0x55, 0x12, 0x20];
        b.extend_from_slice(&self.0);
        b
    }

    /// Whether `data` is this block.
    pub fn matches(&self, data: &[u8]) -> bool {
        Self::of(data) == *self
    }
}

impl std::fmt::Display for Cid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "b{}", base32(&self.to_bytes()))
    }
}

impl std::fmt::Debug for Cid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Cid({self})")
    }
}

impl std::str::FromStr for Cid {
    type Err = Error;
    fn from_str(s: &str) -> Result<Self> {
        let bad = || Error::Corrupt(format!("not a raw sha2-256 CIDv1: {s}"));
        let b = unbase32(s.strip_prefix('b').ok_or_else(bad)?).ok_or_else(bad)?;
        if b.len() != 36 || b[..4] != [0x01, 0x55, 0x12, 0x20] {
            return Err(bad());
        }
        Ok(Self(b[4..].try_into().expect("32 bytes")))
    }
}

impl Serialize for Cid {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Cid {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(serde::de::Error::custom)
    }
}

const B32: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

fn base32(data: &[u8]) -> String {
    let mut out = String::new();
    let (mut buf, mut bits) = (0u32, 0);
    for &b in data {
        buf = (buf << 8) | b as u32;
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(B32[((buf >> bits) & 31) as usize] as char);
        }
    }
    if bits > 0 {
        out.push(B32[((buf << (5 - bits)) & 31) as usize] as char);
    }
    out
}

fn unbase32(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let (mut buf, mut bits) = (0u32, 0);
    for c in s.bytes() {
        let v = B32.iter().position(|x| *x == c)? as u32;
        buf = (buf << 5) | v;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
        }
    }
    Some(out)
}

mod hexser {
    use serde::{Deserialize, Deserializer, Serializer};
    pub fn serialize<S: Serializer>(b: &[u8], s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&super::hex(b))
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(d)?;
        super::unhex(&s).ok_or_else(|| serde::de::Error::custom("bad hex"))
    }
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

// ---------------------------------------------------------------------------------------------
// Keys

/// A recipient's X25519 secret key (keep it private).
#[derive(Clone)]
pub struct SecretKey(<Kem as hpke::Kem>::PrivateKey);

/// A recipient's X25519 public key (register it, e.g. in `ComputeMarket`).
#[derive(Clone, PartialEq, Eq)]
pub struct PublicKey(<Kem as hpke::Kem>::PublicKey);

impl SecretKey {
    /// A fresh key pair.
    pub fn generate() -> (Self, PublicKey) {
        let (sk, pk) = Kem::gen_keypair(&mut UnwrapErr(OsRng));
        (Self(sk), PublicKey(pk))
    }

    /// Deterministic key pair from 32+ bytes of secret input (e.g. derived from a wallet key).
    pub fn derive(ikm: &[u8]) -> (Self, PublicKey) {
        let (sk, pk) = Kem::derive_keypair(ikm);
        (Self(sk), PublicKey(pk))
    }

    /// 32 bytes.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes().into()
    }

    /// From 32 bytes.
    pub fn from_bytes(b: &[u8]) -> Result<Self> {
        <Kem as hpke::Kem>::PrivateKey::from_bytes(b)
            .map(Self)
            .map_err(|_| Error::Params("secret key must be 32 bytes".into()))
    }

    /// Its public key.
    pub fn public_key(&self) -> PublicKey {
        PublicKey(Kem::sk_to_pk(&self.0))
    }
}

impl PublicKey {
    /// 32 bytes.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes().into()
    }

    /// From 32 bytes.
    pub fn from_bytes(b: &[u8]) -> Result<Self> {
        <Kem as hpke::Kem>::PublicKey::from_bytes(b)
            .map(Self)
            .map_err(|_| Error::Params("public key must be 32 bytes".into()))
    }
}

impl std::fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PublicKey({})", hex(&self.to_bytes()))
    }
}

/// A file's content key.
#[derive(Clone, PartialEq, Eq)]
pub struct ContentKey([u8; 32]);

impl ContentKey {
    fn random() -> Self {
        let mut k = [0u8; 32];
        UnwrapErr(OsRng).fill_bytes(&mut k);
        Self(k)
    }

    /// Raw key (for disclosure to a dispute panel, ADR 0011).
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0
    }

    /// From raw bytes.
    pub fn from_bytes(b: &[u8]) -> Result<Self> {
        Ok(Self(b.try_into().map_err(|_| Error::Params("content key must be 32 bytes".into()))?))
    }

    fn cipher(&self) -> XChaCha20Poly1305 {
        XChaCha20Poly1305::new((&self.0).into())
    }
}

impl std::fmt::Debug for ContentKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ContentKey(..)")
    }
}

// ---------------------------------------------------------------------------------------------
// Format

/// The public part of a sealed file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Envelope {
    /// Format version.
    pub v: u32,
    /// Encrypted manifest block.
    pub manifest: Cid,
    /// Nonce the manifest was encrypted with (24 bytes).
    #[serde(with = "hexser")]
    pub nonce: Vec<u8>,
    /// The content key, wrapped for each recipient (no recipient identifiers: a recipient finds
    /// its entry by trying them).
    pub keys: Vec<WrappedKey>,
}

/// A content key wrapped for one recipient (HPKE base mode, X25519 / HKDF-SHA256 /
/// ChaCha20-Poly1305).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WrappedKey {
    /// Encapsulated key.
    #[serde(with = "hexser")]
    pub enc: Vec<u8>,
    /// Wrapped content key.
    #[serde(with = "hexser")]
    pub ct: Vec<u8>,
}

/// What only recipients see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Format version.
    pub v: u32,
    /// File name.
    pub name: String,
    /// Media type.
    pub mime: String,
    /// Exact size in bytes.
    pub size: u64,
    /// Plaintext bytes per chunk.
    pub chunk: u32,
    /// Chunks (a multiple of `k`).
    pub chunks: u32,
    /// Data shards per stripe.
    pub k: u8,
    /// Parity shards per stripe.
    pub m: u8,
    /// First 16 bytes of every chunk nonce (the last 8 are the chunk index).
    #[serde(with = "hexser")]
    pub nonce_prefix: Vec<u8>,
    /// Shards, stripe after stripe: `k` data then `m` parity.
    pub shards: Vec<Cid>,
}

impl Manifest {
    /// Stripes.
    pub fn stripes(&self) -> usize {
        self.chunks as usize / self.k as usize
    }
}

/// An IPFS raw block to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Block {
    /// Its CID.
    pub cid: Cid,
    /// Its bytes.
    pub data: Vec<u8>,
}

impl Block {
    fn new(data: Vec<u8>) -> Self {
        Self { cid: Cid::of(&data), data }
    }
}

/// Result of [`seal`].
#[derive(Debug, Clone)]
pub struct Sealed {
    /// Envelope block (its CID names the file).
    pub envelope: Block,
    /// Encrypted manifest block.
    pub manifest: Block,
    /// Shard blocks, in manifest order.
    pub shards: Vec<Block>,
    /// The content key (to wrap for more recipients later, see [`add_recipient`]).
    pub key: ContentKey,
}

impl Sealed {
    /// Every block to store: envelope, manifest, shards.
    pub fn blocks(&self) -> impl Iterator<Item = &Block> {
        [&self.envelope, &self.manifest].into_iter().chain(&self.shards)
    }
}

/// Options for [`seal`].
#[derive(Debug, Clone)]
pub struct Options {
    /// Data shards per stripe (1..=32).
    pub k: usize,
    /// Parity shards per stripe (0..=32).
    pub m: usize,
}

impl Default for Options {
    fn default() -> Self {
        Self { k: DEFAULT_K, m: DEFAULT_M }
    }
}

/// Chunk size and chunk count for `size` bytes: sizes are padded to buckets so ciphertext
/// reveals only a range. Small files: `k` chunks of 4 KiB, 16 KiB, 64 KiB or 256 KiB; larger ones
/// a multiple of `k` chunks of 256 KiB.
pub fn layout(size: u64, k: usize) -> (usize, usize) {
    let mut chunk = MIN_CHUNK;
    while chunk < MAX_CHUNK && (chunk * k) as u64 <= size {
        chunk *= 4;
    }
    let stripe = (chunk * k) as u64;
    let stripes = size.div_ceil(stripe).max(1) as usize;
    (chunk, stripes * k)
}

fn chunk_nonce(prefix: &[u8], index: u64) -> XNonce {
    let mut n = [0u8; 24];
    n[..16].copy_from_slice(prefix);
    n[16..].copy_from_slice(&index.to_be_bytes());
    n.into()
}

fn chunk_aad(chunks: u32) -> Vec<u8> {
    [CHUNK_AAD, &chunks.to_be_bytes()].concat()
}

fn wrap(key: &ContentKey, to: &PublicKey) -> WrappedKey {
    let (enc, ct) = hpke::single_shot_seal::<WrapAead, Kdf, Kem, _>(
        &OpModeS::Base,
        &to.0,
        KEY_INFO,
        &key.0,
        &[],
        &mut UnwrapErr(OsRng),
    )
    .expect("sealing 32 bytes cannot fail");
    WrappedKey { enc: enc.to_bytes().to_vec(), ct }
}

fn unwrap(w: &WrappedKey, sk: &SecretKey) -> Option<ContentKey> {
    let enc = <Kem as hpke::Kem>::EncappedKey::from_bytes(&w.enc).ok()?;
    let k = hpke::single_shot_open::<WrapAead, Kdf, Kem>(
        &OpModeR::Base,
        &sk.0,
        &enc,
        KEY_INFO,
        &w.ct,
        &[],
    )
    .ok()?;
    ContentKey::from_bytes(&k).ok()
}

/// Seals `data` for `recipients`.
pub fn seal(
    data: &[u8],
    name: &str,
    mime: &str,
    recipients: &[PublicKey],
    opts: &Options,
) -> Result<Sealed> {
    let (k, m) = (opts.k, opts.m);
    if !(1..=32).contains(&k) || m > 32 {
        return Err(Error::Params(format!("k = {k}, m = {m}")));
    }
    if recipients.is_empty() {
        return Err(Error::Params("no recipients".into()));
    }
    let key = ContentKey::random();
    let (chunk, chunks) = layout(data.len() as u64, k);
    let mut prefix = [0u8; 16];
    UnwrapErr(OsRng).fill_bytes(&mut prefix);
    let cipher = key.cipher();
    let aad = chunk_aad(chunks as u32);
    let rs = if m > 0 {
        Some(ReedSolomon::new(k, m).map_err(|e| Error::Params(e.to_string()))?)
    } else {
        None
    };

    let mut shards = Vec::with_capacity(chunks / k * (k + m));
    for stripe in 0..chunks / k {
        let mut group: Vec<Vec<u8>> = Vec::with_capacity(k + m);
        for j in 0..k {
            let index = stripe * k + j;
            let mut plain = vec![0u8; chunk];
            let start = (index * chunk).min(data.len());
            let end = ((index + 1) * chunk).min(data.len());
            plain[..end - start].copy_from_slice(&data[start..end]);
            let ct = cipher
                .encrypt(&chunk_nonce(&prefix, index as u64), Payload { msg: &plain, aad: &aad })
                .expect("encryption cannot fail");
            group.push(ct);
        }
        if let Some(rs) = &rs {
            group.extend((0..m).map(|_| vec![0u8; chunk + TAG]));
            rs.encode(&mut group).map_err(|e| Error::Params(e.to_string()))?;
        }
        shards.extend(group.into_iter().map(Block::new));
    }

    let manifest = Manifest {
        v: VERSION,
        name: name.to_owned(),
        mime: mime.to_owned(),
        size: data.len() as u64,
        chunk: chunk as u32,
        chunks: chunks as u32,
        k: k as u8,
        m: m as u8,
        nonce_prefix: prefix.to_vec(),
        shards: shards.iter().map(|b| b.cid).collect(),
    };
    let mut nonce = [0u8; 24];
    UnwrapErr(OsRng).fill_bytes(&mut nonce);
    let plain = serde_json::to_vec(&manifest).expect("manifest serializes");
    let sealed_manifest = cipher
        .encrypt(&nonce.into(), Payload { msg: &plain, aad: MANIFEST_AAD })
        .expect("encryption cannot fail");
    let manifest_block = Block::new(sealed_manifest);
    let envelope = Envelope {
        v: VERSION,
        manifest: manifest_block.cid,
        nonce: nonce.to_vec(),
        keys: recipients.iter().map(|r| wrap(&key, r)).collect(),
    };
    let envelope_block = Block::new(serde_json::to_vec(&envelope).expect("envelope serializes"));
    Ok(Sealed { envelope: envelope_block, manifest: manifest_block, shards, key })
}

/// Parses an envelope block.
pub fn parse_envelope(data: &[u8]) -> Result<Envelope> {
    let e: Envelope =
        serde_json::from_slice(data).map_err(|e| Error::Corrupt(format!("envelope: {e}")))?;
    if e.v != VERSION || e.nonce.len() != 24 {
        return Err(Error::Corrupt("unsupported envelope".into()));
    }
    Ok(e)
}

/// The content key, if `sk` is one of the recipients.
pub fn open_key(envelope: &Envelope, sk: &SecretKey) -> Result<ContentKey> {
    envelope.keys.iter().find_map(|w| unwrap(w, sk)).ok_or(Error::NotRecipient)
}

/// A copy of the envelope that also lets `to` read the file (e.g. the executor, once a job is
/// matched: no re-upload of the data). Only a holder of the content key can do this.
pub fn add_recipient(envelope: &Envelope, key: &ContentKey, to: &PublicKey) -> Block {
    let mut e = envelope.clone();
    e.keys.push(wrap(key, to));
    Block::new(serde_json::to_vec(&e).expect("envelope serializes"))
}

/// Decrypts the manifest block.
pub fn open_manifest(
    envelope: &Envelope,
    manifest_block: &[u8],
    key: &ContentKey,
) -> Result<Manifest> {
    if !envelope.manifest.matches(manifest_block) {
        return Err(Error::Corrupt("manifest block does not match its CID".into()));
    }
    let nonce: [u8; 24] = envelope
        .nonce
        .as_slice()
        .try_into()
        .map_err(|_| Error::Corrupt("envelope nonce".into()))?;
    let plain = key
        .cipher()
        .decrypt(&nonce.into(), Payload { msg: manifest_block, aad: MANIFEST_AAD })
        .map_err(|_| Error::Corrupt("manifest fails authentication".into()))?;
    let m: Manifest =
        serde_json::from_slice(&plain).map_err(|e| Error::Corrupt(format!("manifest: {e}")))?;
    let (k, chunks) = (m.k as usize, m.chunks as usize);
    let (chunk, expected_chunks) = layout(m.size, k);
    if m.v != VERSION
        || k == 0
        || m.nonce_prefix.len() != 16
        || m.chunk as usize != chunk
        || chunks != expected_chunks
        || m.shards.len() != chunks / k * (k + m.m as usize)
    {
        return Err(Error::Corrupt("inconsistent manifest".into()));
    }
    Ok(m)
}

/// Reassembles the file from the shards that could be fetched (`None` for missing ones, in
/// manifest order). Shards that do not match their CID count as missing; each stripe needs any
/// `k` intact shards.
pub fn recover(
    manifest: &Manifest,
    key: &ContentKey,
    shards: &[Option<Vec<u8>>],
) -> Result<Vec<u8>> {
    let (k, m) = (manifest.k as usize, manifest.m as usize);
    let per = k + m;
    if shards.len() != manifest.shards.len() {
        return Err(Error::Params("one entry per shard expected".into()));
    }
    let chunk = manifest.chunk as usize;
    let aad = chunk_aad(manifest.chunks);
    let cipher = key.cipher();
    let rs = if m > 0 {
        Some(ReedSolomon::new(k, m).map_err(|e| Error::Params(e.to_string()))?)
    } else {
        None
    };
    let mut out = Vec::with_capacity(manifest.chunks as usize * chunk);
    for stripe in 0..manifest.stripes() {
        let mut group: Vec<Option<Vec<u8>>> = (0..per)
            .map(|j| {
                let i = stripe * per + j;
                shards[i]
                    .as_ref()
                    .filter(|d| d.len() == chunk + TAG && manifest.shards[i].matches(d))
                    .cloned()
            })
            .collect();
        let have = group.iter().filter(|s| s.is_some()).count();
        if group[..k].iter().any(Option::is_none) {
            if have < k {
                return Err(Error::TooFewShards { stripe, have, need: k });
            }
            rs.as_ref()
                .expect("missing data shards imply parity")
                .reconstruct_data(&mut group)
                .map_err(|e| Error::Corrupt(e.to_string()))?;
        }
        for (j, ct) in group.into_iter().take(k).enumerate() {
            let index = (stripe * k + j) as u64;
            let ct = ct.expect("reconstructed");
            let plain = cipher
                .decrypt(
                    &chunk_nonce(&manifest.nonce_prefix, index),
                    Payload { msg: &ct, aad: &aad },
                )
                .map_err(|_| Error::Corrupt(format!("chunk {index} fails authentication")))?;
            out.extend_from_slice(&plain);
        }
    }
    out.truncate(manifest.size as usize);
    Ok(out)
}

/// Convenience: opens a file whose blocks `get` returns by CID (`None` when unavailable).
pub fn open(
    envelope_block: &[u8],
    sk: &SecretKey,
    get: impl Fn(&Cid) -> Option<Vec<u8>>,
) -> Result<(Manifest, Vec<u8>)> {
    let envelope = parse_envelope(envelope_block)?;
    let key = open_key(&envelope, sk)?;
    let mblock =
        get(&envelope.manifest).ok_or_else(|| Error::Corrupt("manifest block missing".into()))?;
    let manifest = open_manifest(&envelope, &mblock, &key)?;
    let shards: Vec<Option<Vec<u8>>> = manifest.shards.iter().map(&get).collect();
    let data = recover(&manifest, &key, &shards)?;
    Ok((manifest, data))
}

#[cfg(test)]
mod tests;
