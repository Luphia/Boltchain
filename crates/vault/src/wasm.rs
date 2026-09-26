//! Browser / Node.js bindings (`wasm-bindgen`). Build:
//!
//! ```sh
//! cargo build -p bolt-vault --release --target wasm32-unknown-unknown --features wasm
//! wasm-bindgen --target web --out-dir pkg target/wasm32-unknown-unknown/release/bolt_vault.wasm
//! ```
//!
//! Blocks are passed as `{ cid: string, bytes: Uint8Array }`; fetching and storing them (IPFS,
//! a gateway, SwarmStorage's backend) is up to the caller.

use crate::{Cid, ContentKey, Options, PublicKey, SecretKey};
use js_sys::{Array, Object, Reflect, Uint8Array};
use wasm_bindgen::prelude::*;

fn err(e: impl std::fmt::Display) -> JsError {
    JsError::new(&e.to_string())
}

fn set(o: &Object, k: &str, v: impl Into<JsValue>) {
    Reflect::set(o, &k.into(), &v.into()).expect("setting a property on a plain object");
}

fn block(b: &crate::Block) -> Object {
    let o = Object::new();
    set(&o, "cid", b.cid.to_string());
    set(&o, "bytes", Uint8Array::from(b.data.as_slice()));
    o
}

/// A new X25519 key pair: `{ secretKey, publicKey }` (32 bytes each).
#[wasm_bindgen(js_name = generateKeyPair)]
pub fn generate_key_pair() -> Object {
    let (sk, pk) = SecretKey::generate();
    let o = Object::new();
    set(&o, "secretKey", Uint8Array::from(&sk.to_bytes()[..]));
    set(&o, "publicKey", Uint8Array::from(&pk.to_bytes()[..]));
    o
}

/// Public key of a secret key.
#[wasm_bindgen(js_name = publicKeyOf)]
pub fn public_key_of(secret_key: &[u8]) -> Result<Uint8Array, JsError> {
    Ok(Uint8Array::from(
        &SecretKey::from_bytes(secret_key).map_err(err)?.public_key().to_bytes()[..],
    ))
}

/// CID (CIDv1, raw, sha2-256) of a block.
#[wasm_bindgen(js_name = cidOf)]
pub fn cid_of(bytes: &[u8]) -> String {
    Cid::of(bytes).to_string()
}

/// Seals a file for `recipients` (array of 32-byte public keys). Returns
/// `{ envelope, manifest, shards: [...], contentKey }`; store every block, share the envelope CID.
#[wasm_bindgen]
pub fn seal(
    data: &[u8],
    name: &str,
    mime: &str,
    recipients: Array,
    k: Option<u8>,
    m: Option<u8>,
) -> Result<Object, JsError> {
    let recipients = recipients
        .iter()
        .map(|v| PublicKey::from_bytes(&Uint8Array::new(&v).to_vec()))
        .collect::<crate::Result<Vec<_>>>()
        .map_err(err)?;
    let opts = Options {
        k: k.map(usize::from).unwrap_or(crate::DEFAULT_K),
        m: m.map(usize::from).unwrap_or(crate::DEFAULT_M),
    };
    let s = crate::seal(data, name, mime, &recipients, &opts).map_err(err)?;
    let o = Object::new();
    set(&o, "envelope", block(&s.envelope));
    set(&o, "manifest", block(&s.manifest));
    set(&o, "shards", s.shards.iter().map(block).collect::<Array>());
    set(&o, "contentKey", Uint8Array::from(&s.key.to_bytes()[..]));
    Ok(o)
}

/// Opens an envelope with a recipient's secret key: `{ manifestCid, contentKey }`.
#[wasm_bindgen(js_name = openEnvelope)]
pub fn open_envelope(envelope: &[u8], secret_key: &[u8]) -> Result<Object, JsError> {
    let e = crate::parse_envelope(envelope).map_err(err)?;
    let key = crate::open_key(&e, &SecretKey::from_bytes(secret_key).map_err(err)?).map_err(err)?;
    let o = Object::new();
    set(&o, "manifestCid", e.manifest.to_string());
    set(&o, "contentKey", Uint8Array::from(&key.to_bytes()[..]));
    Ok(o)
}

/// Decrypts the manifest: `{ name, mime, size, chunk, chunks, k, m, shards: [cid, ...] }`.
#[wasm_bindgen(js_name = openManifest)]
pub fn open_manifest(
    envelope: &[u8],
    manifest: &[u8],
    content_key: &[u8],
) -> Result<JsValue, JsError> {
    let e = crate::parse_envelope(envelope).map_err(err)?;
    let key = ContentKey::from_bytes(content_key).map_err(err)?;
    let m = crate::open_manifest(&e, manifest, &key).map_err(err)?;
    let o = serde_wasm_bindgen::to_value(&m).map_err(err)?;
    Reflect::set(&o, &"json".into(), &serde_json::to_string(&m).map_err(err)?.into())
        .map_err(|_| JsError::new("manifest object"))?;
    Ok(o)
}

/// Reassembles the file. `shards` has one entry per manifest shard: `Uint8Array`, or `null` when
/// it could not be fetched. `manifest_json` is the `json` field returned by `openManifest`.
#[wasm_bindgen]
pub fn recover(
    manifest_json: &str,
    content_key: &[u8],
    shards: Array,
) -> Result<Uint8Array, JsError> {
    let m: crate::Manifest = serde_json::from_str(manifest_json).map_err(err)?;
    let key = ContentKey::from_bytes(content_key).map_err(err)?;
    let shards: Vec<Option<Vec<u8>>> = shards
        .iter()
        .map(|v| {
            if v.is_null() || v.is_undefined() { None } else { Some(Uint8Array::new(&v).to_vec()) }
        })
        .collect();
    let data = crate::recover(&m, &key, &shards).map_err(err)?;
    Ok(Uint8Array::from(data.as_slice()))
}

/// A new envelope that also lets `public_key` read the file (after a job is matched).
#[wasm_bindgen(js_name = addRecipient)]
pub fn add_recipient(
    envelope: &[u8],
    content_key: &[u8],
    public_key: &[u8],
) -> Result<Object, JsError> {
    let e = crate::parse_envelope(envelope).map_err(err)?;
    let key = ContentKey::from_bytes(content_key).map_err(err)?;
    let pk = PublicKey::from_bytes(public_key).map_err(err)?;
    Ok(block(&crate::add_recipient(&e, &key, &pk)))
}
