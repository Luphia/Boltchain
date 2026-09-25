//! BLS12-381 keys and signatures (public keys in G1, signatures in G2, as on Ethereum), proofs of
//! possession, and the binding between a validator's BLS key and its fee-recipient address.

use alloy_primitives::{Address, B256, FixedBytes, Signature, eip191_hash_message};
use blst::{BLST_ERROR, min_pk};

/// Compressed G1 public key.
pub type BlsPublicKey = FixedBytes<48>;
/// Compressed G2 signature.
pub type BlsSignature = FixedBytes<96>;

/// Signature DST (the proof-of-possession ciphersuite from the IETF BLS draft, as Ethereum).
pub const SIG_DST: &[u8] = b"BLS_SIG_BLS12381G2_XMD:SHA-256_SSWU_RO_POP_";
/// Proof-of-possession DST.
pub const POP_DST: &[u8] = b"BLS_POP_BLS12381G2_XMD:SHA-256_SSWU_RO_POP_";

/// A BLS secret key.
#[derive(Clone)]
pub struct BlsSecretKey(min_pk::SecretKey);

impl std::fmt::Debug for BlsSecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "BlsSecretKey({})", self.public_key())
    }
}

impl BlsSecretKey {
    /// Derives a key from at least 32 bytes of input key material (IETF KeyGen).
    pub fn from_ikm(ikm: &[u8]) -> Option<Self> {
        min_pk::SecretKey::key_gen(ikm, &[]).ok().map(Self)
    }

    /// Generates a fresh random key.
    pub fn random() -> Self {
        let mut ikm = [0u8; 32];
        getrandom(&mut ikm);
        Self::from_ikm(&ikm).expect("32 bytes of IKM")
    }

    /// Big-endian scalar encoding.
    pub fn to_bytes(&self) -> [u8; 32] {
        self.0.to_bytes()
    }

    /// Parses a big-endian scalar.
    pub fn from_bytes(b: &[u8]) -> Option<Self> {
        min_pk::SecretKey::from_bytes(b).ok().map(Self)
    }

    /// Compressed public key.
    pub fn public_key(&self) -> BlsPublicKey {
        BlsPublicKey::from(self.0.sk_to_pk().compress())
    }

    /// Signs `msg`.
    pub fn sign(&self, msg: &[u8]) -> BlsSignature {
        BlsSignature::from(self.0.sign(msg, SIG_DST, &[]).compress())
    }

    /// Proof of possession: a signature over the public key under the PoP DST.
    pub fn proof_of_possession(&self) -> BlsSignature {
        BlsSignature::from(self.0.sign(self.public_key().as_slice(), POP_DST, &[]).compress())
    }
}

fn getrandom(buf: &mut [u8]) {
    use std::io::Read;
    // /dev/urandom is available on every platform Boltchain targets (Linux, macOS).
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(buf))
        .expect("reading /dev/urandom");
}

/// EIP-2537 encoding of a public key: x || y, each padded to 64 bytes (128 bytes). `None` if the
/// key is invalid.
pub fn pubkey_point(pk_bytes: &BlsPublicKey) -> Option<[u8; 128]> {
    let raw = pk(pk_bytes)?.serialize(); // x || y, 48 bytes each
    let mut out = [0u8; 128];
    out[16..64].copy_from_slice(&raw[0..48]);
    out[80..128].copy_from_slice(&raw[48..96]);
    Some(out)
}

/// EIP-2537 encoding of a signature: x.c0 || x.c1 || y.c0 || y.c1, each padded to 64 bytes
/// (256 bytes). blst serializes G2 as x.c1 || x.c0 || y.c1 || y.c0. `None` if invalid.
pub fn signature_point(s: &BlsSignature) -> Option<[u8; 256]> {
    let raw = sig(s)?.serialize();
    let mut out = [0u8; 256];
    for (i, src) in [48usize, 0, 144, 96].into_iter().enumerate() {
        out[64 * i + 16..64 * (i + 1)].copy_from_slice(&raw[src..src + 48]);
    }
    Some(out)
}

fn pk(pk: &BlsPublicKey) -> Option<min_pk::PublicKey> {
    // key_validate checks the encoding, that the point is not infinity and is in the subgroup.
    min_pk::PublicKey::key_validate(pk.as_slice()).ok()
}

fn sig(s: &BlsSignature) -> Option<min_pk::Signature> {
    min_pk::Signature::sig_validate(s.as_slice(), true).ok()
}

/// Whether `pk` is a valid, non-identity G1 point in the prime-order subgroup.
pub fn is_valid_public_key(p: &BlsPublicKey) -> bool {
    pk(p).is_some()
}

/// Verifies a single signature.
pub fn verify(p: &BlsPublicKey, msg: &[u8], s: &BlsSignature) -> bool {
    match (pk(p), sig(s)) {
        (Some(p), Some(s)) => {
            s.verify(false, msg, SIG_DST, &[], &p, false) == BLST_ERROR::BLST_SUCCESS
        }
        _ => false,
    }
}

/// Verifies a proof of possession.
pub fn verify_possession(p: &BlsPublicKey, pop: &BlsSignature) -> bool {
    match (pk(p), sig(pop)) {
        (Some(pk_), Some(s)) => {
            s.verify(false, p.as_slice(), POP_DST, &[], &pk_, false) == BLST_ERROR::BLST_SUCCESS
        }
        _ => false,
    }
}

/// Aggregates signatures (all over the same message, for a QC).
pub fn aggregate(sigs: &[BlsSignature]) -> Option<BlsSignature> {
    let parsed: Vec<min_pk::Signature> = sigs.iter().map(sig).collect::<Option<_>>()?;
    let refs: Vec<&min_pk::Signature> = parsed.iter().collect();
    let agg = min_pk::AggregateSignature::aggregate(&refs, false).ok()?;
    Some(BlsSignature::from(agg.to_signature().compress()))
}

/// Verifies an aggregate signature by `pks` over one message. Callers must only pass keys whose
/// proof of possession was checked (genesis or staking contract), which rules out rogue-key
/// attacks.
pub fn fast_aggregate_verify(pks: &[BlsPublicKey], msg: &[u8], agg: &BlsSignature) -> bool {
    let Some(parsed) = pks.iter().map(pk).collect::<Option<Vec<_>>>() else { return false };
    let refs: Vec<&min_pk::PublicKey> = parsed.iter().collect();
    match sig(agg) {
        Some(s) => s.fast_aggregate_verify(false, msg, SIG_DST, &refs) == BLST_ERROR::BLST_SUCCESS,
        None => false,
    }
}

/// Text a validator's fee-recipient address signs (EIP-191 `personal_sign`) to claim a BLS key.
pub fn binding_message(chain_id: u64, bls: &BlsPublicKey, address: &Address) -> String {
    format!(
        "Boltchain validator key binding\nchain id: {chain_id}\nBLS public key: {bls}\naddress: {}",
        address.to_checksum(None)
    )
}

/// EIP-191 hash of the binding message.
pub fn binding_hash(chain_id: u64, bls: &BlsPublicKey, address: &Address) -> B256 {
    eip191_hash_message(binding_message(chain_id, bls, address))
}

/// Verifies that `signature` (65 bytes, r || s || v) over the binding message was made by
/// `address`.
pub fn verify_binding(
    chain_id: u64,
    bls: &BlsPublicKey,
    address: &Address,
    signature: &[u8],
) -> bool {
    let Ok(sig) = Signature::try_from(signature) else { return false };
    sig.recover_address_from_prehash(&binding_hash(chain_id, bls, address))
        .is_ok_and(|a| a == *address)
}

/// Signs the binding message with a secp256k1 secret key (the fee-recipient address's key).
/// Returns the 65-byte `r || s || v` signature, the same bytes `personal_sign` produces.
pub fn sign_binding(
    eth_secret: &[u8; 32],
    chain_id: u64,
    bls: &BlsPublicKey,
) -> Option<(Address, alloy_primitives::Bytes)> {
    let sk = k256::ecdsa::SigningKey::from_slice(eth_secret).ok()?;
    let address = Address::from_public_key(sk.verifying_key());
    let hash = binding_hash(chain_id, bls, &address);
    let (s, rec) = sk.sign_prehash_recoverable(hash.as_slice()).ok()?;
    let sig = Signature::from_signature_and_parity(s, rec.is_y_odd());
    Some((address, sig.as_bytes().to_vec().into()))
}

/// Deterministic development secp256k1 key `index` for fee-recipient addresses. **Insecure.**
pub fn dev_eth_key(index: u32) -> [u8; 32] {
    alloy_primitives::keccak256(format!("boltchain-insecure-dev-fee-recipient-{index}")).0
}

/// Deterministic development key `index`. **Insecure**: anyone can derive it. Used only by dev
/// genesis files and tests.
pub fn dev_key(index: u32) -> BlsSecretKey {
    let ikm = alloy_primitives::keccak256(format!("boltchain-insecure-dev-validator-{index}"));
    BlsSecretKey::from_ikm(ikm.as_slice()).expect("32 bytes")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_verify_aggregate() {
        let keys: Vec<_> = (0..5).map(dev_key).collect();
        let msg = b"round 7 block 0xabc";
        let sigs: Vec<_> = keys.iter().map(|k| k.sign(msg)).collect();
        for (k, s) in keys.iter().zip(&sigs) {
            assert!(verify(&k.public_key(), msg, s));
            assert!(!verify(&k.public_key(), b"other", s));
        }
        let agg = aggregate(&sigs).unwrap();
        let pks: Vec<_> = keys.iter().map(|k| k.public_key()).collect();
        assert!(fast_aggregate_verify(&pks, msg, &agg));
        assert!(!fast_aggregate_verify(&pks[..4], msg, &agg));
    }

    #[test]
    fn possession_and_key_validity() {
        let k = BlsSecretKey::random();
        assert!(is_valid_public_key(&k.public_key()));
        assert!(verify_possession(&k.public_key(), &k.proof_of_possession()));
        // A plain signature over the key bytes is not a PoP (different DST).
        assert!(!verify_possession(&k.public_key(), &k.sign(k.public_key().as_slice())));
        assert!(!is_valid_public_key(&BlsPublicKey::repeat_byte(1)));
        assert!(!is_valid_public_key(&BlsPublicKey::ZERO));
        assert_eq!(BlsSecretKey::from_bytes(&k.to_bytes()).unwrap().public_key(), k.public_key());
    }

    #[test]
    fn binding_signature() {
        let bls = dev_key(0).public_key();
        let (addr, bytes) = sign_binding(&[7u8; 32], 8017, &bls).unwrap();
        assert!(verify_binding(8017, &bls, &addr, &bytes));
        assert!(!verify_binding(1, &bls, &addr, &bytes), "chain id is part of the message");
        assert!(!verify_binding(8017, &dev_key(1).public_key(), &addr, &bytes));
    }
}
