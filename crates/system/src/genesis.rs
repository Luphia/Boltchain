//! Genesis state: the user `alloc`, the Osaka protocol predeploys, and the Boltchain system
//! contracts deployed at fixed addresses (ADR 0006 §7).
//!
//! Deployment runs each contract's init code *at its final address* (the account's code is set to
//! the init code and called as the system address), so `address(this)`, storage written by the
//! constructor and immutables are all correct. The returned runtime code then replaces the init
//! code. Initialisation calls (Safe setup, bootstrap validators, parameters) follow, also from the
//! system address at block 0.

use crate::{abi::*, addresses::*, artifacts};
use alloy_consensus::Header;
use alloy_primitives::{Address, B256, Bytes, U256, keccak256};
use alloy_sol_types::{SolCall, SolValue};
use alloy_trie::root::state_root_unhashed;
use bolt_primitives::{Genesis, genesis::GenesisAccount, params::MIN_COMMITTEE_SIZE};
use revm::{
    Context, MainBuilder, MainContext, SystemCallCommitEvm,
    bytecode::Bytecode,
    context::result::ExecutionResult,
    database::{CacheDB, EmptyDB},
    state::AccountInfo,
};
use std::{
    collections::{BTreeMap, HashMap},
    sync::{Mutex, OnceLock},
};

/// Why the genesis state could not be built.
#[derive(Debug, thiserror::Error)]
pub enum GenesisBuildError {
    /// A constructor or initialisation call failed.
    #[error("genesis call to {0} failed: {1}")]
    Call(Address, String),
}

struct Builder {
    db: CacheDB<EmptyDB>,
    chain_id: u64,
    timestamp: u64,
}

impl Builder {
    fn run(&mut self, to: Address, data: Bytes) -> Result<Bytes, GenesisBuildError> {
        let mut block = revm::context::BlockEnv {
            number: U256::ZERO,
            timestamp: U256::from(self.timestamp),
            ..Default::default()
        };
        block.prevrandao = Some(B256::ZERO);
        let mut evm = Context::mainnet()
            .with_db(&mut self.db)
            .with_cfg(bolt_exec::cfg_env(self.chain_id))
            .with_block(block)
            .build_mainnet();
        match evm.system_call_commit(to, data) {
            Ok(ExecutionResult::Success { output, .. }) => Ok(output.into_data()),
            Ok(other) => Err(GenesisBuildError::Call(to, format!("{other:?}"))),
            Err(e) => Err(GenesisBuildError::Call(to, format!("{e:?}"))),
        }
    }

    /// Runs `initcode ‖ args` at `addr` and installs the returned runtime code there.
    fn deploy(
        &mut self,
        addr: Address,
        initcode: &Bytes,
        args: &[u8],
    ) -> Result<(), GenesisBuildError> {
        let init = Bytecode::new_raw([initcode.as_ref(), args].concat().into());
        self.db.insert_account_info(
            addr,
            AccountInfo {
                nonce: 1,
                code_hash: init.hash_slow(),
                code: Some(init),
                ..Default::default()
            },
        );
        let runtime = self.run(addr, Bytes::new())?;
        let code = Bytecode::new_raw(runtime);
        let mut info = self.db.load_account(addr).map(|a| a.info.clone()).unwrap_or_default();
        info.code_hash = code.hash_slow();
        info.code = Some(code);
        info.nonce = 1;
        self.db.insert_account_info(addr, info);
        Ok(())
    }

    fn call<C: SolCall>(&mut self, to: Address, call: C) -> Result<C::Return, GenesisBuildError> {
        let out = self.run(to, call.abi_encode().into())?;
        C::abi_decode_returns(&out).map_err(|e| GenesisBuildError::Call(to, e.to_string()))
    }

    fn into_alloc(self) -> BTreeMap<Address, GenesisAccount> {
        let mut out = BTreeMap::new();
        for (addr, acc) in self.db.cache.accounts {
            let code = match &acc.info.code {
                Some(c) if !c.is_empty() => c.original_bytes(),
                _ => self
                    .db
                    .cache
                    .contracts
                    .get(&acc.info.code_hash)
                    .map(|c| c.original_bytes())
                    .unwrap_or_default(),
            };
            let storage: BTreeMap<B256, B256> = acc
                .storage
                .iter()
                .filter(|(_, v)| !v.is_zero())
                .map(|(k, v)| (B256::from(*k), B256::from(*v)))
                .collect();
            if acc.info.nonce == 0
                && acc.info.balance.is_zero()
                && code.is_empty()
                && storage.is_empty()
            {
                continue; // e.g. the system address touched by calls
            }
            out.insert(
                addr,
                GenesisAccount { nonce: acc.info.nonce, balance: acc.info.balance, code, storage },
            );
        }
        out
    }
}

fn build(g: &Genesis) -> Result<BTreeMap<Address, GenesisAccount>, GenesisBuildError> {
    let mut b = Builder {
        db: CacheDB::new(EmptyDB::default()),
        chain_id: g.config.chain_id,
        timestamp: g.timestamp,
    };
    let mut supply = U256::ZERO;
    for (addr, acc) in g.effective_alloc() {
        supply += acc.balance;
        let mut info = AccountInfo { nonce: acc.nonce, balance: acc.balance, ..Default::default() };
        if !acc.code.is_empty() {
            let code = Bytecode::new_raw(acc.code.clone());
            info.code_hash = code.hash_slow();
            info.code = Some(code);
        }
        b.db.insert_account_info(addr, info);
        for (k, v) in &acc.storage {
            b.db.insert_account_storage(addr, U256::from_be_bytes(k.0), U256::from_be_bytes(v.0))
                .expect("cache db");
        }
    }

    // Implementations, then their proxies.
    b.deploy(STAKING_IMPL, &artifacts::staking_manager().bytecode, &[])?;
    b.deploy(CONSENSUS_IMPL, &artifacts::consensus_registry().bytecode, &[])?;
    b.deploy(REWARDS_IMPL, &artifacts::reward_distributor().bytecode, &[])?;
    b.deploy(PARAMS_IMPL, &artifacts::param_registry().bytecode, &[])?;
    b.deploy(HISTORY_IMPL, &artifacts::history_registry().bytecode, &[])?;
    let proxy = &artifacts::system_proxy().bytecode;
    for (p, i) in [
        (STAKING, STAKING_IMPL),
        (CONSENSUS, CONSENSUS_IMPL),
        (REWARDS, REWARDS_IMPL),
        (PARAMS, PARAMS_IMPL),
        (HISTORY, HISTORY_IMPL),
    ] {
        b.deploy(p, proxy, &i.abi_encode())?;
    }

    // Governance: Safe (proxy -> 1.4.1 singleton) and two timelocks proposed by it.
    b.deploy(SAFE_SINGLETON, &artifacts::safe().bytecode, &[])?;
    b.deploy(SAFE_FALLBACK, &artifacts::safe_fallback().bytecode, &[])?;
    b.deploy(SAFE, &artifacts::safe_proxy().bytecode, &SAFE_SINGLETON.abi_encode())?;
    let gov = &g.governance;
    b.call(
        SAFE,
        ISafe::setupCall {
            owners: gov.owners.clone(),
            threshold: U256::from(gov.threshold),
            to: Address::ZERO,
            data: Bytes::new(),
            fallbackHandler: SAFE_FALLBACK,
            paymentToken: Address::ZERO,
            payment: U256::ZERO,
            paymentReceiver: Address::ZERO,
        },
    )?;
    let timelock = &artifacts::timelock().bytecode;
    for (addr, delay) in
        [(UPGRADE_TIMELOCK, gov.upgrade_delay_seconds), (PARAM_TIMELOCK, gov.param_delay_seconds)]
    {
        // proposers (and cancellers): the Safe; executors: anyone (address 0); no extra admin.
        let args =
            (U256::from(delay), vec![SAFE], vec![Address::ZERO], Address::ZERO).abi_encode_params();
        b.deploy(addr, timelock, &args)?;
    }

    // Parameters, bootstrap validators and their committee (epochs 0 and 1), supply.
    let floor = g.config.committee_size.min(MIN_COMMITTEE_SIZE);
    b.call(
        PARAMS,
        IParamRegistry::initializeCall {
            gasLimit: g.config.gas_limit,
            minBaseFee: g.config.min_base_fee_wei,
            committeeSize: g.config.committee_size,
            floor,
        },
    )?;
    let mut ids = Vec::new();
    for v in &g.bootstrap_validators {
        let id = b.call(
            STAKING,
            IStakingManager::registerBootstrapCall {
                pubkey: Bytes::copy_from_slice(v.bls_pubkey.as_slice()),
                feeRecipient: v.fee_recipient,
            },
        )?;
        ids.push(id);
    }
    let committee = crate::committee::Committee::equal(&ids);
    b.call(
        CONSENSUS,
        IConsensusRegistry::initializeCall {
            memberIds: committee.members.clone(),
            weights: committee.weights.clone(),
            seats: committee.seats_bytes(),
        },
    )?;
    b.call(REWARDS, IRewardDistributor::initializeCall { genesisSupply: supply })?;
    Ok(b.into_alloc())
}

fn cache() -> &'static Mutex<HashMap<B256, BTreeMap<Address, GenesisAccount>>> {
    static C: OnceLock<Mutex<HashMap<B256, BTreeMap<Address, GenesisAccount>>>> = OnceLock::new();
    C.get_or_init(Default::default)
}

/// The complete genesis state (cached per genesis file).
pub fn genesis_alloc(g: &Genesis) -> Result<BTreeMap<Address, GenesisAccount>, GenesisBuildError> {
    let key = keccak256(serde_json::to_vec(g).expect("genesis serializes"));
    if let Some(a) = cache().lock().expect("genesis cache").get(&key) {
        return Ok(a.clone());
    }
    let alloc = build(g)?;
    cache().lock().expect("genesis cache").insert(key, alloc.clone());
    Ok(alloc)
}

/// Genesis state root.
pub fn genesis_state_root(g: &Genesis) -> Result<B256, GenesisBuildError> {
    Ok(state_root_unhashed(genesis_alloc(g)?.iter().map(|(a, acc)| (*a, acc.trie_account()))))
}

/// The genesis header.
pub fn genesis_header(g: &Genesis) -> Result<Header, GenesisBuildError> {
    Ok(g.header_with_state_root(genesis_state_root(g)?))
}

/// The genesis block hash.
pub fn genesis_hash(g: &Genesis) -> Result<B256, GenesisBuildError> {
    Ok(genesis_header(g)?.hash_slow())
}
