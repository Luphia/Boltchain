//! Genesis file format, validation and genesis header construction.
//!
//! Boltchain has no genesis token allocation: every account in `alloc` must have a zero balance.
//! `alloc` exists only to predeploy contract code (system contracts, the governance multisig).
//!
//! Local development chains (`"dev": true`) are the one exception: they may fund accounts, and to
//! keep their transactions from being replayable on Boltchain they must use a chain id other
//! than 8017.

use crate::params::*;
use alloy_consensus::{
    Header,
    constants::{EMPTY_OMMER_ROOT_HASH, EMPTY_ROOT_HASH, KECCAK_EMPTY},
};
use alloy_eips::{
    eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE},
    eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE},
    eip7685::EMPTY_REQUESTS_HASH,
};
use alloy_primitives::{Address, B64, B256, Bloom, Bytes, U256, keccak256};
use alloy_trie::{TrieAccount, root::storage_root_unhashed};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub use crate::bls::{BlsPublicKey, BlsSignature};

/// Seconds in one day, used for timelock bounds.
const DAY: u64 = 86_400;

/// Genesis file.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Genesis {
    /// Local development chain: allows funded accounts, forbids chain id 8017.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub dev: bool,
    /// Chain parameters fixed at genesis.
    pub config: ChainConfig,
    /// Genesis timestamp (unix seconds). Slot 0 starts here.
    pub timestamp: u64,
    /// Header extra data, at most 32 bytes.
    #[serde(default)]
    pub extra_data: Bytes,
    /// Bootstrap-phase validators. They hold no tokens; they only sign blocks until the
    /// bootstrap exit thresholds are met.
    pub bootstrap_validators: Vec<BootstrapValidator>,
    /// Initial governance: multisig owners and timelock delays.
    pub governance: Governance,
    /// Predeployed contracts. Balances must be zero.
    #[serde(default)]
    pub alloc: BTreeMap<Address, GenesisAccount>,
}

/// Chain parameters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ChainConfig {
    /// EIP-155 chain id.
    pub chain_id: u64,
    /// Slot length in seconds.
    pub slot_seconds: u64,
    /// Slots per epoch.
    pub epoch_slots: u64,
    /// Committee seats per epoch.
    pub committee_size: u32,
    /// Initial block gas limit.
    pub gas_limit: u64,
    /// Minimum base fee in wei.
    pub min_base_fee_wei: u64,
    /// Dev chains only: bootstrap exit threshold on the number of stakers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap_exit_min_stakers: Option<u32>,
    /// Dev chains only: bootstrap exit threshold on total stake, in whole BOLT.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bootstrap_exit_min_stake_bolt: Option<u64>,
}

impl Default for ChainConfig {
    fn default() -> Self {
        Self {
            chain_id: CHAIN_ID,
            slot_seconds: SLOT_SECONDS,
            epoch_slots: EPOCH_SLOTS,
            committee_size: MIN_COMMITTEE_SIZE,
            gas_limit: DEFAULT_GAS_LIMIT,
            min_base_fee_wei: MIN_BASE_FEE_WEI,
            bootstrap_exit_min_stakers: None,
            bootstrap_exit_min_stake_bolt: None,
        }
    }
}

impl ChainConfig {
    /// Bootstrap exit thresholds: (stakers, total stake in whole BOLT).
    pub fn bootstrap_exit(&self) -> (u32, u64) {
        (
            self.bootstrap_exit_min_stakers.unwrap_or(BOOTSTRAP_EXIT_MIN_STAKERS),
            self.bootstrap_exit_min_stake_bolt.unwrap_or(BOOTSTRAP_EXIT_MIN_TOTAL_STAKE_BOLT),
        )
    }
}

/// A bootstrap-phase validator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BootstrapValidator {
    /// Operator name, for humans.
    pub name: String,
    /// BLS12-381 public key used for votes and the BLS-VRF.
    pub bls_pubkey: BlsPublicKey,
    /// Proof of possession of the BLS key (rules out rogue-key attacks on aggregate signatures).
    pub proof_of_possession: BlsSignature,
    /// Address that receives this validator's (discounted, locked) rewards.
    pub fee_recipient: Address,
    /// EIP-191 signature by `fee_recipient` over [`crate::bls::binding_message`], proving the
    /// address holder claims this BLS key. Required outside dev chains.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding_signature: Option<Bytes>,
}

/// Initial governance configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Governance {
    /// Multisig owners.
    pub owners: Vec<Address>,
    /// Signatures required.
    pub threshold: u32,
    /// Timelock for system-contract upgrades and security parameters, in seconds.
    pub upgrade_delay_seconds: u64,
    /// Timelock for bounded parameter tweaks, in seconds.
    pub param_delay_seconds: u64,
}

/// A predeployed account.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GenesisAccount {
    /// Account nonce.
    #[serde(default)]
    pub nonce: u64,
    /// Balance. Must be zero: Boltchain has no genesis allocation.
    #[serde(default)]
    pub balance: U256,
    /// Contract code.
    #[serde(default)]
    pub code: Bytes,
    /// Initial storage.
    #[serde(default)]
    pub storage: BTreeMap<B256, B256>,
}

impl GenesisAccount {
    /// The account as stored in the state trie.
    pub fn trie_account(&self) -> TrieAccount {
        let storage = self
            .storage
            .iter()
            .filter(|(_, v)| !v.is_zero())
            .map(|(k, v)| (*k, U256::from_be_bytes(v.0)));
        TrieAccount {
            nonce: self.nonce,
            balance: self.balance,
            storage_root: storage_root_unhashed(storage),
            code_hash: if self.code.is_empty() { KECCAK_EMPTY } else { keccak256(&self.code) },
        }
    }
}

/// Reasons a genesis file is rejected.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GenesisError {
    #[error("chain id must be {CHAIN_ID}, got {0}")]
    ChainId(u64),
    #[error("dev chains must not use chain id {CHAIN_ID}")]
    DevChainId,
    #[error("slot/epoch length must be {SLOT_SECONDS}s/{EPOCH_SLOTS} slots")]
    SlotTiming,
    #[error("committee size {0} is below the floor of {MIN_COMMITTEE_SIZE}")]
    CommitteeTooSmall(u32),
    #[error("gas limit {0} outside allowed range")]
    GasLimit(u64),
    #[error("min base fee {0} below protocol floor {MIN_BASE_FEE_WEI}")]
    BaseFee(u64),
    #[error("extra data is {0} bytes, max 32")]
    ExtraData(usize),
    #[error("need at least {MIN_BOOTSTRAP_VALIDATORS} bootstrap validators, got {0}")]
    TooFewValidators(usize),
    #[error("duplicate or invalid BLS public key: {0}")]
    BadValidatorKey(BlsPublicKey),
    #[error("invalid proof of possession for BLS key {0}")]
    BadPossession(BlsPublicKey),
    #[error("missing fee-recipient binding signature for BLS key {0}")]
    MissingBinding(BlsPublicKey),
    #[error("binding signature for BLS key {0} was not made by its fee recipient")]
    BadBinding(BlsPublicKey),
    #[error("multisig owners must be unique and non-zero")]
    BadOwners,
    #[error("threshold {threshold} invalid for {owners} owners (needs a strict majority)")]
    Threshold { threshold: u32, owners: usize },
    #[error("upgrade timelock must be at least the unbonding period plus 2 days")]
    UpgradeDelay,
    #[error("param timelock must be at least 2 days and not exceed the upgrade timelock")]
    ParamDelay,
    #[error("no genesis allocation: account {0} has a non-zero balance")]
    NonZeroBalance(Address),
    #[error("bootstrap exit overrides are for dev chains only")]
    DevOnly,
    #[error("account {0} is a protocol predeploy and cannot be overridden")]
    ReservedAddress(Address),
}

impl Genesis {
    /// Parses and validates a genesis JSON document.
    pub fn from_json(json: &str) -> Result<Self, GenesisLoadError> {
        let genesis: Self = serde_json::from_str(json)?;
        genesis.validate()?;
        Ok(genesis)
    }

    /// Checks every protocol rule a genesis must satisfy.
    pub fn validate(&self) -> Result<(), GenesisError> {
        let c = &self.config;
        if self.dev && c.chain_id == CHAIN_ID {
            return Err(GenesisError::DevChainId);
        }
        if !self.dev && c.chain_id != CHAIN_ID {
            return Err(GenesisError::ChainId(c.chain_id));
        }
        if self.dev {
            // Dev chains may use short epochs, small committees and low bootstrap thresholds
            // (vote counters are 16-bit per epoch, committees at most 4096 seats).
            if c.slot_seconds == 0 || !(2..=65_535).contains(&c.epoch_slots) {
                return Err(GenesisError::SlotTiming);
            }
            if !(1..=4096).contains(&c.committee_size) {
                return Err(GenesisError::CommitteeTooSmall(c.committee_size));
            }
        } else {
            if c.slot_seconds != SLOT_SECONDS || c.epoch_slots != EPOCH_SLOTS {
                return Err(GenesisError::SlotTiming);
            }
            if c.committee_size < MIN_COMMITTEE_SIZE || c.committee_size > 4096 {
                return Err(GenesisError::CommitteeTooSmall(c.committee_size));
            }
            if c.bootstrap_exit_min_stakers.is_some() || c.bootstrap_exit_min_stake_bolt.is_some() {
                return Err(GenesisError::DevOnly);
            }
        }
        if c.gas_limit < GAS_LIMIT_RANGE.0 || c.gas_limit > GAS_LIMIT_RANGE.1 {
            return Err(GenesisError::GasLimit(c.gas_limit));
        }
        if c.min_base_fee_wei < MIN_BASE_FEE_WEI {
            return Err(GenesisError::BaseFee(c.min_base_fee_wei));
        }
        if self.extra_data.len() > 32 {
            return Err(GenesisError::ExtraData(self.extra_data.len()));
        }

        if self.bootstrap_validators.len() < MIN_BOOTSTRAP_VALIDATORS {
            return Err(GenesisError::TooFewValidators(self.bootstrap_validators.len()));
        }
        let mut keys = BTreeSet::new();
        for v in &self.bootstrap_validators {
            if !crate::bls::is_valid_public_key(&v.bls_pubkey) || !keys.insert(v.bls_pubkey) {
                return Err(GenesisError::BadValidatorKey(v.bls_pubkey));
            }
            if !crate::bls::verify_possession(&v.bls_pubkey, &v.proof_of_possession) {
                return Err(GenesisError::BadPossession(v.bls_pubkey));
            }
            match &v.binding_signature {
                Some(sig)
                    if !crate::bls::verify_binding(
                        c.chain_id,
                        &v.bls_pubkey,
                        &v.fee_recipient,
                        sig,
                    ) =>
                {
                    return Err(GenesisError::BadBinding(v.bls_pubkey));
                }
                Some(_) => {}
                None if !self.dev => return Err(GenesisError::MissingBinding(v.bls_pubkey)),
                None => {}
            }
        }

        let g = &self.governance;
        let owners: BTreeSet<_> = g.owners.iter().collect();
        if owners.len() != g.owners.len() || owners.iter().any(|o| o.is_zero()) {
            return Err(GenesisError::BadOwners);
        }
        let n = g.owners.len();
        if n == 0 || (g.threshold as usize) * 2 <= n || g.threshold as usize > n {
            return Err(GenesisError::Threshold { threshold: g.threshold, owners: n });
        }
        if g.upgrade_delay_seconds < (UNBONDING_EPOCHS + 2) * DAY {
            return Err(GenesisError::UpgradeDelay);
        }
        if g.param_delay_seconds < 2 * DAY || g.param_delay_seconds > g.upgrade_delay_seconds {
            return Err(GenesisError::ParamDelay);
        }

        for (addr, acc) in &self.alloc {
            if !self.dev && !acc.balance.is_zero() {
                return Err(GenesisError::NonZeroBalance(*addr));
            }
            // Protocol predeploys and the system contract range 0xB017000000000000000000000000000000xxxx.
            let system = addr[0] == 0xb0 && addr[1] == 0x17 && addr[2..18].iter().all(|b| *b == 0);
            if protocol_predeploys().contains_key(addr) || system {
                return Err(GenesisError::ReservedAddress(*addr));
            }
        }
        Ok(())
    }

    /// `alloc` plus the protocol predeploys every Osaka chain needs (EIP-4788, EIP-2935).
    pub fn effective_alloc(&self) -> BTreeMap<Address, GenesisAccount> {
        let mut all = self.alloc.clone();
        all.extend(protocol_predeploys());
        all
    }

    /// Initial randomness seed: keccak of the concatenated bootstrap BLS keys, in genesis order.
    pub fn initial_seed(&self) -> B256 {
        let mut buf = Vec::with_capacity(48 * self.bootstrap_validators.len());
        for v in &self.bootstrap_validators {
            buf.extend_from_slice(v.bls_pubkey.as_slice());
        }
        keccak256(buf)
    }

    /// The genesis block header for a given state root, with every Osaka-era field populated. The
    /// state root depends on the system contracts, computed by `bolt_system::genesis_header`.
    pub fn header_with_state_root(&self, state_root: B256) -> Header {
        Header {
            parent_hash: B256::ZERO,
            ommers_hash: EMPTY_OMMER_ROOT_HASH,
            beneficiary: Address::ZERO,
            state_root,
            transactions_root: EMPTY_ROOT_HASH,
            receipts_root: EMPTY_ROOT_HASH,
            logs_bloom: Bloom::ZERO,
            difficulty: U256::ZERO,
            number: 0,
            gas_limit: self.config.gas_limit,
            gas_used: 0,
            timestamp: self.timestamp,
            extra_data: self.extra_data.clone(),
            mix_hash: self.initial_seed(),
            nonce: B64::ZERO,
            base_fee_per_gas: Some(self.config.min_base_fee_wei),
            withdrawals_root: Some(EMPTY_ROOT_HASH),
            blob_gas_used: Some(0),
            excess_blob_gas: Some(0),
            // Carries the parent QC hash on later blocks; genesis has no parent.
            parent_beacon_block_root: Some(B256::ZERO),
            requests_hash: Some(EMPTY_REQUESTS_HASH),
            // Amsterdam fields: not part of Osaka.
            block_access_list_hash: None,
            slot_number: None,
        }
    }
}

/// Protocol-level predeploys, identical to Ethereum mainnet deployments.
pub fn protocol_predeploys() -> BTreeMap<Address, GenesisAccount> {
    let acc = |code: &Bytes| GenesisAccount { nonce: 1, code: code.clone(), ..Default::default() };
    BTreeMap::from([
        (BEACON_ROOTS_ADDRESS, acc(&BEACON_ROOTS_CODE)),
        (HISTORY_STORAGE_ADDRESS, acc(&HISTORY_STORAGE_CODE)),
    ])
}

/// Error loading a genesis file.
#[derive(Debug, thiserror::Error)]
pub enum GenesisLoadError {
    #[error("invalid genesis JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("invalid genesis: {0}")]
    Invalid(#[from] GenesisError),
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_primitives::address;

    fn sample() -> Genesis {
        Genesis {
            dev: false,
            config: ChainConfig::default(),
            timestamp: 1_800_000_000,
            extra_data: Bytes::from_static(b"Boltchain"),
            bootstrap_validators: (0..7u32).map(|i| dev_validator(i, CHAIN_ID)).collect(),
            governance: Governance {
                owners: (1..=9u8).map(|i| Address::repeat_byte(0x10 + i)).collect(),
                threshold: 5,
                upgrade_delay_seconds: 16 * DAY,
                param_delay_seconds: 2 * DAY,
            },
            alloc: BTreeMap::new(),
        }
    }

    fn dev_validator(i: u32, chain_id: u64) -> BootstrapValidator {
        let k = crate::bls::dev_key(i);
        let pk = k.public_key();
        let (fee_recipient, sig) =
            crate::bls::sign_binding(&crate::bls::dev_eth_key(i), chain_id, &pk).unwrap();
        BootstrapValidator {
            name: format!("v{i}"),
            bls_pubkey: pk,
            proof_of_possession: k.proof_of_possession(),
            fee_recipient,
            binding_signature: Some(sig),
        }
    }

    #[test]
    fn validator_keys_are_checked() {
        let mut g = sample();
        g.bootstrap_validators[2].proof_of_possession =
            g.bootstrap_validators[3].proof_of_possession;
        assert!(matches!(g.validate(), Err(GenesisError::BadPossession(_))));

        let mut g = sample();
        g.bootstrap_validators[2].binding_signature =
            g.bootstrap_validators[3].binding_signature.clone();
        assert!(matches!(g.validate(), Err(GenesisError::BadBinding(_))));

        let mut g = sample();
        g.bootstrap_validators[2].binding_signature = None;
        assert!(matches!(g.validate(), Err(GenesisError::MissingBinding(_))));
        // Dev chains may omit bindings.
        g.dev = true;
        g.config.chain_id = 1337;
        for v in &mut g.bootstrap_validators {
            v.binding_signature = None;
        }
        g.validate().unwrap();

        let mut g = sample();
        g.bootstrap_validators[0].bls_pubkey = BlsPublicKey::repeat_byte(1);
        assert!(matches!(g.validate(), Err(GenesisError::BadValidatorKey(_))));
    }

    #[test]
    fn sample_is_valid_and_hash_is_stable() {
        let g = sample();
        g.validate().unwrap();
        let root = B256::repeat_byte(1);
        assert_eq!(
            g.header_with_state_root(root).hash_slow(),
            g.clone().header_with_state_root(root).hash_slow()
        );
        let h = g.header_with_state_root(root);
        assert_eq!(h.requests_hash, Some(EMPTY_REQUESTS_HASH));
        assert_eq!(h.withdrawals_root, Some(EMPTY_ROOT_HASH));
    }

    #[test]
    fn json_roundtrip() {
        let g = sample();
        let json = serde_json::to_string_pretty(&g).unwrap();
        assert_eq!(Genesis::from_json(&json).unwrap(), g);
    }

    #[test]
    fn rejects_genesis_allocation() {
        let mut g = sample();
        let a = address!("00000000000000000000000000000000000000aa");
        g.alloc.insert(a, GenesisAccount { balance: U256::from(1), ..Default::default() });
        assert_eq!(g.validate(), Err(GenesisError::NonZeroBalance(a)));
    }

    #[test]
    fn dev_chains_may_fund_accounts_but_not_use_8017() {
        let mut g = sample();
        g.dev = true;
        g.alloc.insert(
            Address::repeat_byte(0xaa),
            GenesisAccount { balance: U256::from(1), ..Default::default() },
        );
        assert_eq!(g.validate(), Err(GenesisError::DevChainId));
        g.config.chain_id = 1337;
        // Bindings name the chain id, so the 8017 ones no longer verify...
        assert!(matches!(g.validate(), Err(GenesisError::BadBinding(_))));
        // ...and dev chains may simply omit them.
        for v in &mut g.bootstrap_validators {
            v.binding_signature = None;
        }
        g.validate().unwrap();
    }

    #[test]
    fn rejects_rule_violations() {
        let mut g = sample();
        g.config.chain_id = 1;
        assert_eq!(g.validate(), Err(GenesisError::ChainId(1)));

        let mut g = sample();
        g.config.committee_size = 511;
        assert!(matches!(g.validate(), Err(GenesisError::CommitteeTooSmall(511))));

        let mut g = sample();
        g.bootstrap_validators.pop();
        assert!(matches!(g.validate(), Err(GenesisError::TooFewValidators(6))));

        let mut g = sample();
        g.bootstrap_validators[1].bls_pubkey = g.bootstrap_validators[0].bls_pubkey;
        assert!(matches!(g.validate(), Err(GenesisError::BadValidatorKey(_))));

        let mut g = sample();
        g.governance.threshold = 4;
        assert!(matches!(g.validate(), Err(GenesisError::Threshold { .. })));

        let mut g = sample();
        g.governance.upgrade_delay_seconds = 14 * DAY;
        assert_eq!(g.validate(), Err(GenesisError::UpgradeDelay));

        let mut g = sample();
        g.alloc.insert(BEACON_ROOTS_ADDRESS, GenesisAccount::default());
        assert_eq!(g.validate(), Err(GenesisError::ReservedAddress(BEACON_ROOTS_ADDRESS)));
    }

    /// Header layout pinned against an independent Python implementation (pyrlp), for the M0
    /// state root (before system contracts; py_ecc and eth_account checked the PoPs and
    /// bindings). The full genesis hash, which now includes the system contracts, is pinned in
    /// `bolt-system`.
    #[test]
    fn devnet_genesis_header_is_pinned() {
        let g = Genesis::from_json(include_str!("../../../genesis/devnet.json")).unwrap();
        let expect = |s: &str| s.parse::<B256>().unwrap();
        let root = expect("0x9f42bd8694bb51cea140f07dae2ee2a6a9af552101474651939d3ecdcc895863");
        assert_eq!(
            g.header_with_state_root(root).hash_slow(),
            expect("0xbf1c712896002f51051519f4df32e94cd9294a38036479587ecce630aff32275")
        );
    }

    /// The mainnet template carries the real validator and multisig addresses but no BLS keys
    /// yet: it must fail validation on the missing keys and pass once they are filled in.
    #[test]
    fn mainnet_template_only_lacks_bls_keys() {
        let mut g: Genesis =
            serde_json::from_str(include_str!("../../../genesis/mainnet.template.json")).unwrap();
        assert!(matches!(g.validate(), Err(GenesisError::BadValidatorKey(k)) if k.is_zero()));
        for (i, v) in g.bootstrap_validators.iter_mut().enumerate() {
            let k = crate::bls::dev_key(100 + i as u32);
            v.bls_pubkey = k.public_key();
            v.proof_of_possession = k.proof_of_possession();
        }
        // With keys in place, the only thing still missing is each operator's binding signature,
        // which only the holders of the CAFECA fee-recipient addresses can produce.
        assert!(matches!(g.validate(), Err(GenesisError::MissingBinding(_))));
        assert_eq!(g.governance.owners.len(), 9);
        assert_eq!(g.governance.threshold, 5);
        assert_eq!(g.bootstrap_validators.len(), 7);
    }

    #[test]
    fn storage_affects_the_trie_account() {
        let mut acc = GenesisAccount { code: Bytes::from_static(&[0x00]), ..Default::default() };
        assert_eq!(acc.trie_account().storage_root, EMPTY_ROOT_HASH);
        acc.storage.insert(B256::ZERO, B256::with_last_byte(1));
        assert_ne!(acc.trie_account().storage_root, EMPTY_ROOT_HASH);
    }
}
