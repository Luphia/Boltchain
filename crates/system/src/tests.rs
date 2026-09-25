//! Contract tests on an in-memory revm database built from the genesis state.

use crate::{abi::*, addresses::*, artifacts, genesis_alloc};
use alloy_primitives::{Address, B256, Bytes, TxKind, U256, keccak256};
use alloy_sol_types::SolCall;
use bolt_primitives::{
    Genesis,
    bls::{self, BlsSecretKey, dev_key, pubkey_point, signature_point},
};
use revm::{
    Context, ExecuteCommitEvm, MainBuilder, MainContext, SystemCallCommitEvm,
    bytecode::Bytecode,
    context::{BlockEnv, TxEnv, result::ExecutionResult},
    database::{CacheDB, EmptyDB},
    state::AccountInfo,
};

pub(crate) struct Evm {
    pub db: CacheDB<EmptyDB>,
    pub chain_id: u64,
    pub number: u64,
    pub timestamp: u64,
}

impl Evm {
    pub fn from_genesis(g: &Genesis) -> Self {
        let mut db = CacheDB::new(EmptyDB::default());
        for (a, acc) in genesis_alloc(g).unwrap() {
            let mut info =
                AccountInfo { nonce: acc.nonce, balance: acc.balance, ..Default::default() };
            if !acc.code.is_empty() {
                let c = Bytecode::new_raw(acc.code.clone());
                info.code_hash = c.hash_slow();
                info.code = Some(c);
            }
            db.insert_account_info(a, info);
            for (k, v) in acc.storage {
                db.insert_account_storage(a, U256::from_be_bytes(k.0), U256::from_be_bytes(v.0))
                    .unwrap();
            }
        }
        Self { db, chain_id: g.config.chain_id, number: 1, timestamp: g.timestamp }
    }

    fn block(&self) -> BlockEnv {
        BlockEnv {
            number: U256::from(self.number),
            timestamp: U256::from(self.timestamp),
            basefee: 0,
            gas_limit: 1 << 40,
            ..Default::default()
        }
    }

    pub fn fund(&mut self, a: Address, wei: U256) {
        let mut info = self.db.load_account(a).unwrap().info.clone();
        info.balance += wei;
        self.db.insert_account_info(a, info);
    }

    pub fn balance(&mut self, a: Address) -> U256 {
        self.db.load_account(a).unwrap().info.balance
    }

    /// A transaction; returns (success, output).
    pub fn tx(&mut self, from: Address, to: Address, value: U256, data: Vec<u8>) -> (bool, Bytes) {
        let (ok, out, _) = self.tx_gas(from, to, value, data);
        (ok, out)
    }

    /// A transaction; returns (success, output, gas used).
    pub fn tx_gas(
        &mut self,
        from: Address,
        to: Address,
        value: U256,
        data: Vec<u8>,
    ) -> (bool, Bytes, u64) {
        let nonce = self.db.load_account(from).map(|a| a.info.nonce).unwrap_or(0);
        let block = self.block();
        let mut cfg = bolt_exec::cfg_env(self.chain_id);
        cfg.tx_gas_limit_cap = Some(1 << 30);
        let mut evm = Context::mainnet()
            .with_db(&mut self.db)
            .with_cfg(cfg)
            .with_block(block)
            .build_mainnet();
        let tx = TxEnv {
            caller: from,
            kind: TxKind::Call(to),
            value,
            data: data.into(),
            gas_limit: 50_000_000,
            gas_price: 0,
            nonce,
            chain_id: Some(self.chain_id),
            ..Default::default()
        };
        let r = evm.transact_commit(tx).unwrap();
        let gas = r.tx_gas_used();
        match r {
            ExecutionResult::Success { output, .. } => (true, output.into_data(), gas),
            ExecutionResult::Revert { output, .. } => (false, output, gas),
            other => panic!("halted: {other:?}"),
        }
    }

    pub fn call<C: SolCall>(&mut self, from: Address, to: Address, value: U256, c: C) -> C::Return {
        let (ok, out) = self.tx(from, to, value, c.abi_encode());
        assert!(ok, "call to {to} reverted: {out}");
        C::abi_decode_returns(&out).unwrap()
    }

    pub fn try_call<C: SolCall>(&mut self, from: Address, to: Address, value: U256, c: C) -> bool {
        self.tx(from, to, value, c.abi_encode()).0
    }

    pub fn view<C: SolCall>(&mut self, to: Address, c: C) -> C::Return {
        crate::queries::call(&self.db, self.chain_id, to, c).unwrap()
    }

    pub fn system<C: SolCall>(&mut self, to: Address, c: C) -> C::Return {
        let block = self.block();
        let mut evm = Context::mainnet()
            .with_db(&mut self.db)
            .with_cfg(bolt_exec::cfg_env(self.chain_id))
            .with_block(block)
            .build_mainnet();
        match evm.system_call_commit(to, c.abi_encode().into()).unwrap() {
            ExecutionResult::Success { output, .. } => {
                C::abi_decode_returns(&output.into_data()).unwrap()
            }
            other => panic!("system call failed: {other:?}"),
        }
    }

    /// Call as a contract account (e.g. the Safe executing a transaction), bypassing EIP-3607.
    pub fn call_as<C: SolCall>(&mut self, caller: Address, to: Address, c: C) -> bool {
        let block = self.block();
        let mut evm = Context::mainnet()
            .with_db(&mut self.db)
            .with_cfg(bolt_exec::cfg_env(self.chain_id))
            .with_block(block)
            .build_mainnet();
        evm.system_call_with_caller_commit(caller, to, c.abi_encode().into()).unwrap().is_success()
    }

    pub fn deploy_at(&mut self, a: Address, runtime: &Bytes) {
        let c = Bytecode::new_raw(runtime.clone());
        self.db.insert_account_info(
            a,
            AccountInfo { nonce: 1, code_hash: c.hash_slow(), code: Some(c), ..Default::default() },
        );
    }
}

pub(crate) fn dev_genesis() -> Genesis {
    Genesis::from_json(include_str!("../../../genesis/dev.json")).unwrap()
}

fn bytes(b: &[u8]) -> Bytes {
    Bytes::copy_from_slice(b)
}

#[test]
fn solidity_bls_matches_blst() {
    let mut evm = Evm::from_genesis(&dev_genesis());
    let harness = Address::repeat_byte(0x42);
    evm.deploy_at(harness, &artifacts::bls_harness().deployed);
    let me = Address::repeat_byte(1);
    for i in 0..3u32 {
        let sk = dev_key(i);
        let pk = sk.public_key();
        let pkp = pubkey_point(&pk).unwrap();
        // compression matches blst's
        let c = evm.view(harness, IBLSHarness::compressCall { pk: bytes(&pkp) });
        assert_eq!(c.as_ref(), pk.as_slice());
        // signature over a message
        let msg = format!("boltchain test message {i}").into_bytes();
        let sig = sk.sign(&msg);
        let sp = signature_point(&sig).unwrap();
        assert!(evm.view(
            harness,
            IBLSHarness::verifyCall { pk: bytes(&pkp), message: bytes(&msg), sig: bytes(&sp) }
        ));
        assert!(!evm.view(
            harness,
            IBLSHarness::verifyCall { pk: bytes(&pkp), message: bytes(b"other"), sig: bytes(&sp) }
        ));
        let other = pubkey_point(&dev_key(i + 10).public_key()).unwrap();
        assert!(!evm.view(
            harness,
            IBLSHarness::verifyCall { pk: bytes(&other), message: bytes(&msg), sig: bytes(&sp) }
        ));
        // proof of possession uses the PoP DST
        let pop = signature_point(&sk.proof_of_possession()).unwrap();
        assert!(evm.view(
            harness,
            IBLSHarness::verifyPopCall {
                pk: bytes(&pkp),
                pubkey: bytes(pk.as_slice()),
                pop: bytes(&pop)
            }
        ));
        assert!(!evm.view(
            harness,
            IBLSHarness::verifyCall {
                pk: bytes(&pkp),
                message: bytes(pk.as_slice()),
                sig: bytes(&pop)
            }
        ));
    }
    // gas of one verification, as a transaction
    let sk = BlsSecretKey::from_ikm(&[9u8; 32]).unwrap();
    let pkp = pubkey_point(&sk.public_key()).unwrap();
    let sp = signature_point(&sk.sign(b"gas")).unwrap();
    let (ok, _) = evm.tx(
        me,
        harness,
        U256::ZERO,
        IBLSHarness::verifyCall { pk: bytes(&pkp), message: bytes(b"gas"), sig: bytes(&sp) }
            .abi_encode(),
    );
    assert!(ok);
    assert!(bls::verify(&sk.public_key(), b"gas", &sk.sign(b"gas")));
}

#[test]
fn genesis_deploys_system_contracts() {
    let g = dev_genesis();
    let mut evm = Evm::from_genesis(&g);
    // Safe
    let owners = evm.view(SAFE, ISafe::getOwnersCall {});
    assert_eq!(owners, g.governance.owners);
    assert_eq!(evm.view(SAFE, ISafe::getThresholdCall {}), U256::from(g.governance.threshold));
    // Timelocks
    assert_eq!(
        evm.view(UPGRADE_TIMELOCK, ITimelock::getMinDelayCall {}),
        U256::from(g.governance.upgrade_delay_seconds)
    );
    assert_eq!(
        evm.view(PARAM_TIMELOCK, ITimelock::getMinDelayCall {}),
        U256::from(g.governance.param_delay_seconds)
    );
    let proposer = keccak256("PROPOSER_ROLE");
    assert!(evm.view(UPGRADE_TIMELOCK, ITimelock::hasRoleCall { role: proposer, account: SAFE }));
    // Bootstrap committee for epochs 0 and 1
    for e in 0..2 {
        let c = evm.view(CONSENSUS, IConsensusRegistry::committeeCall { epoch: e });
        assert_eq!(c.ids, (1..=g.bootstrap_validators.len() as u32).collect::<Vec<_>>());
    }
    let keys = evm.view(STAKING, IStakingManager::keysOfCall { ids: vec![1, 2] });
    assert_eq!(&keys.pubkeys[..48], g.bootstrap_validators[0].bls_pubkey.as_slice());
    assert_eq!(keys.recipients[1], g.bootstrap_validators[1].fee_recipient);
    // Params and supply
    let p = evm.view(PARAMS, IParamRegistry::paramsCall {});
    assert_eq!(p.gasLimit, g.config.gas_limit);
    assert_eq!(p.committeeSize, g.config.committee_size);
    let funded: U256 = g.alloc.values().map(|a| a.balance).sum();
    assert_eq!(evm.view(REWARDS, IRewardDistributor::supplyCall {}), funded);
    // Genesis-only functions are closed afterwards.
    assert!(!evm.try_call(
        SYSTEM,
        STAKING,
        U256::ZERO,
        IStakingManager::registerBootstrapCall {
            pubkey: bytes(&[0; 48]),
            feeRecipient: Address::ZERO
        }
    ));
}

/// Registers validator `i` (dev key) from `owner` with `stake` wei.
pub(crate) fn register(evm: &mut Evm, i: u32, owner: Address, stake: U256) -> bool {
    let sk = dev_key(i);
    let pk = sk.public_key();
    evm.fund(owner, stake + U256::from(10u64).pow(U256::from(18)));
    evm.try_call(
        owner,
        STAKING,
        stake,
        IStakingManager::registerCall {
            pubkey: bytes(pk.as_slice()),
            pubkeyPoint: bytes(&pubkey_point(&pk).unwrap()),
            pop: bytes(&signature_point(&sk.proof_of_possession()).unwrap()),
            feeRecipient: owner,
        },
    )
}

fn bolt(n: u64) -> U256 {
    U256::from(n) * U256::from(10u64).pow(U256::from(18))
}

#[test]
fn staking_register_exit_withdraw() {
    let mut evm = Evm::from_genesis(&dev_genesis());
    let alice = Address::repeat_byte(0xa1);
    // below the minimum
    assert!(!register(&mut evm, 20, alice, bolt(63)));
    assert!(register(&mut evm, 20, alice, bolt(64)));
    // same key twice
    assert!(!register(&mut evm, 20, Address::repeat_byte(0xa2), bolt(64)));
    // wrong proof of possession
    let (sk, other) = (dev_key(21), dev_key(22));
    evm.fund(alice, bolt(100));
    assert!(!evm.try_call(
        alice,
        STAKING,
        bolt(64),
        IStakingManager::registerCall {
            pubkey: bytes(sk.public_key().as_slice()),
            pubkeyPoint: bytes(&pubkey_point(&sk.public_key()).unwrap()),
            pop: bytes(&signature_point(&other.proof_of_possession()).unwrap()),
            feeRecipient: alice,
        }
    ));
    let id = evm.view(STAKING, IStakingManager::countCall {});
    assert_eq!(id, 8, "7 bootstrap validators + 1");
    let snap = evm.view(STAKING, IStakingManager::snapshotCall {});
    assert_eq!(snap.ids, vec![8]);
    assert_eq!(snap.stakes, vec![bolt(64)]);
    // top-up by anyone, exit by owner only
    evm.fund(Address::repeat_byte(0xb0), bolt(10));
    evm.call(Address::repeat_byte(0xb0), STAKING, bolt(1), IStakingManager::depositCall { id });
    assert!(!evm.try_call(
        Address::repeat_byte(0xb0),
        STAKING,
        U256::ZERO,
        IStakingManager::requestExitCall { id }
    ));
    evm.call(alice, STAKING, U256::ZERO, IStakingManager::requestExitCall { id });
    assert!(evm.view(STAKING, IStakingManager::snapshotCall {}).ids.is_empty());
    // bonded for 1 + 14 epochs
    assert!(!evm.try_call(alice, STAKING, U256::ZERO, IStakingManager::withdrawCall { id }));
    evm.system(
        CONSENSUS,
        IConsensusRegistry::beginEpochCall {
            epoch: 14,
            memberIds: vec![1],
            weights: vec![1],
            seats: bytes(&[0, 0]),
            endBootstrap: false,
        },
    );
    assert!(!evm.try_call(alice, STAKING, U256::ZERO, IStakingManager::withdrawCall { id }));
    evm.system(
        CONSENSUS,
        IConsensusRegistry::beginEpochCall {
            epoch: 15,
            memberIds: vec![1],
            weights: vec![1],
            seats: bytes(&[0, 0]),
            endBootstrap: false,
        },
    );
    let before = evm.balance(alice);
    evm.call(alice, STAKING, U256::ZERO, IStakingManager::withdrawCall { id });
    assert_eq!(evm.balance(alice) - before, bolt(65));
    // pause: only the Safe
    assert!(!evm.try_call(
        alice,
        STAKING,
        U256::ZERO,
        IStakingManager::setDepositsPausedCall { paused: true }
    ));
    assert!(evm.call_as(SAFE, STAKING, IStakingManager::setDepositsPausedCall { paused: true }));
    assert!(!register(&mut evm, 23, alice, bolt(64)));
}

/// Consensus message layouts (duplicated here so the contract test does not depend on the
/// consensus crate; the node-level test checks the real encoder).
fn vote_msg(chain: u64, epoch: u64, round: u64, block: B256) -> Vec<u8> {
    [
        b"boltchain/vote/v2".as_slice(),
        &chain.to_be_bytes(),
        &epoch.to_be_bytes(),
        &round.to_be_bytes(),
        block.as_slice(),
    ]
    .concat()
}

#[test]
fn double_vote_is_slashed() {
    let g = dev_genesis();
    let mut evm = Evm::from_genesis(&g);
    let owners: Vec<Address> = (0..10u8).map(|i| Address::repeat_byte(0x10 + i)).collect();
    for (i, o) in owners.iter().enumerate() {
        assert!(register(&mut evm, 30 + i as u32, *o, bolt(1000)));
    }
    let id = 8u32; // dev key 30
    let sk = dev_key(30);
    let (a, b) = (
        vote_msg(g.config.chain_id, 0, 5, B256::repeat_byte(1)),
        vote_msg(g.config.chain_id, 0, 5, B256::repeat_byte(2)),
    );
    let (sa, sb) = (sk.sign(&a), sk.sign(&b));
    let reporter = Address::repeat_byte(0xee);
    evm.fund(reporter, bolt(1));
    let data =
        crate::evidence::submit_evidence_calldata(id, &sk.public_key(), &a, &sa, &b, &sb).unwrap();
    // wrong validator id: rejected
    let wrong =
        crate::evidence::submit_evidence_calldata(id + 1, &sk.public_key(), &a, &sa, &b, &sb)
            .unwrap();
    assert!(!evm.tx(reporter, CONSENSUS, U256::ZERO, wrong.to_vec()).0);
    // same message twice: not a conflict
    let same =
        crate::evidence::submit_evidence_calldata(id, &sk.public_key(), &a, &sa, &a, &sa).unwrap();
    assert!(!evm.tx(reporter, CONSENSUS, U256::ZERO, same.to_vec()).0);
    // different rounds: not a conflict
    let c = vote_msg(g.config.chain_id, 0, 6, B256::repeat_byte(2));
    let other_round =
        crate::evidence::submit_evidence_calldata(id, &sk.public_key(), &a, &sa, &c, &sk.sign(&c))
            .unwrap();
    assert!(!evm.tx(reporter, CONSENSUS, U256::ZERO, other_round.to_vec()).0);
    // forged second signature
    let forged = crate::evidence::submit_evidence_calldata(
        id,
        &sk.public_key(),
        &a,
        &sa,
        &b,
        &dev_key(31).sign(&b),
    )
    .unwrap();
    assert!(!evm.tx(reporter, CONSENSUS, U256::ZERO, forged.to_vec()).0);

    let supply_before = evm.view(REWARDS, IRewardDistributor::supplyCall {});
    let bps = evm.view(CONSENSUS, IConsensusRegistry::slashBpsCall {});
    assert_eq!(bps, U256::from(500), "5% when nothing was slashed recently");
    let before = evm.balance(reporter);
    let (ok, out) = evm.tx(reporter, CONSENSUS, U256::ZERO, data.to_vec());
    assert!(ok, "{out}");
    let slashed = IConsensusRegistry::submitEvidenceCall::abi_decode_returns(&out).unwrap();
    assert_eq!(slashed, bolt(50));
    assert_eq!(evm.balance(reporter) - before, bolt(50) / U256::from(100));
    assert_eq!(
        supply_before - evm.view(REWARDS, IRewardDistributor::supplyCall {}),
        bolt(50) * U256::from(99) / U256::from(100)
    );
    let v = evm.view(STAKING, IStakingManager::validatorCall { id });
    assert_eq!(v.stake, bolt(950));
    assert!(matches!(v.status, IStakingManager::Status::Slashed));
    assert!(!evm.view(STAKING, IStakingManager::snapshotCall {}).ids.contains(&id));
    // replay rejected
    assert!(!evm.tx(reporter, CONSENSUS, U256::ZERO, data.to_vec()).0);
    // correlated: the next offender pays more (5% + 3 x 50/9000)
    let bps = evm.view(CONSENSUS, IConsensusRegistry::slashBpsCall {});
    assert_eq!(bps, U256::from(500 + 3 * 50 * 10_000 / 9_000));
}

alloy_sol_types::sol! {
    function execute(address target, uint256 value, bytes payload, bytes32 predecessor, bytes32 salt) external payable;
}

#[test]
fn upgrades_go_through_the_16_day_timelock() {
    use alloy_sol_types::SolValue;
    let g = dev_genesis();
    let mut evm = Evm::from_genesis(&g);
    let new_impl = Address::repeat_byte(0x77);
    // a fresh HistoryRegistry implementation (UUPS, immutable __self = its own address)
    evm.deploy_at(new_impl, &artifacts::history_registry().deployed);
    let _ = new_impl;
    // Deploying via init code at the address would set __self correctly; runtime code copied from
    // HISTORY_IMPL has __self = HISTORY_IMPL, so use the genesis builder path instead:
    let fresh = Address::repeat_byte(0x78);
    let init = Bytecode::new_raw(artifacts::history_registry().bytecode.clone());
    evm.db.insert_account_info(
        fresh,
        AccountInfo {
            nonce: 1,
            code_hash: init.hash_slow(),
            code: Some(init),
            ..Default::default()
        },
    );
    let runtime = {
        let block = evm.block();
        let mut e = Context::mainnet()
            .with_db(&mut evm.db)
            .with_cfg(bolt_exec::cfg_env(evm.chain_id))
            .with_block(block)
            .build_mainnet();
        match e.system_call_commit(fresh, Bytes::new()).unwrap() {
            ExecutionResult::Success { output, .. } => output.into_data(),
            o => panic!("{o:?}"),
        }
    };
    evm.deploy_at(fresh, &runtime);

    let upgrade =
        IUpgradeable::upgradeToAndCallCall { newImplementation: fresh, data: Bytes::new() }
            .abi_encode();
    // Not even the Safe can upgrade directly.
    assert!(!evm.call_as(
        SAFE,
        HISTORY,
        IUpgradeable::upgradeToAndCallCall { newImplementation: fresh, data: Bytes::new() }
    ));
    // Schedule through the upgrade timelock (only the Safe may propose).
    let delay = U256::from(g.governance.upgrade_delay_seconds);
    let schedule = ITimelock::scheduleCall {
        target: HISTORY,
        value: U256::ZERO,
        data: upgrade.clone().into(),
        predecessor: B256::ZERO,
        salt: B256::ZERO,
        delay,
    };
    assert!(!evm.call_as(Address::repeat_byte(5), UPGRADE_TIMELOCK, schedule.clone()));
    assert!(evm.call_as(SAFE, UPGRADE_TIMELOCK, schedule));
    // shorter than the minimum delay is refused
    let quick = ITimelock::scheduleCall {
        target: HISTORY,
        value: U256::ZERO,
        data: upgrade.clone().into(),
        predecessor: B256::ZERO,
        salt: B256::repeat_byte(1),
        delay: U256::from(3600),
    };
    assert!(!evm.call_as(SAFE, UPGRADE_TIMELOCK, quick));
    let execute = executeCall {
        target: HISTORY,
        value: U256::ZERO,
        payload: upgrade.into(),
        predecessor: B256::ZERO,
        salt: B256::ZERO,
    };
    let anyone = Address::repeat_byte(0x99);
    evm.fund(anyone, U256::from(10u64).pow(U256::from(18)));
    assert!(!evm.try_call(anyone, UPGRADE_TIMELOCK, U256::ZERO, execute.clone()), "too early");
    evm.timestamp += g.governance.upgrade_delay_seconds;
    assert!(
        evm.try_call(anyone, UPGRADE_TIMELOCK, U256::ZERO, execute),
        "after the delay anyone may execute"
    );
    let slot = U256::from_be_bytes(
        alloy_primitives::b256!(
            "0x360894a13ba1a3210667c828492db98dca3e2076cc3735a920a3ca505d382bbc"
        )
        .0,
    );
    let imp = revm::DatabaseRef::storage_ref(&evm.db, HISTORY, slot).unwrap();
    assert_eq!(Address::from_word(imp.into()), fresh);
    let _ = (fresh,).abi_encode();

    // Parameters: the 2-day timelock can move them within bounds only.
    assert!(evm.call_as(PARAM_TIMELOCK, PARAMS, IParamRegistry::setGasLimitCall { v: 40_000_000 }));
    assert!(!evm.call_as(
        PARAM_TIMELOCK,
        PARAMS,
        IParamRegistry::setGasLimitCall { v: 70_000_000 }
    ));
    assert!(!evm.call_as(SAFE, PARAMS, IParamRegistry::setGasLimitCall { v: 40_000_000 }));
    // committee size: 16-day timelock only, never below the genesis floor
    let floor = g.config.committee_size.min(bolt_primitives::params::MIN_COMMITTEE_SIZE);
    assert!(!evm.call_as(
        PARAM_TIMELOCK,
        PARAMS,
        IParamRegistry::setCommitteeSizeCall { v: floor + 1 }
    ));
    assert!(!evm.call_as(
        UPGRADE_TIMELOCK,
        PARAMS,
        IParamRegistry::setCommitteeSizeCall { v: floor - 1 }
    ));
    assert!(evm.call_as(
        UPGRADE_TIMELOCK,
        PARAMS,
        IParamRegistry::setCommitteeSizeCall { v: floor + 1 }
    ));
}

/// Genesis now includes the compiled system contracts: any change to them, to the deployment
/// procedure or to genesis construction changes this hash. Update it deliberately (and note the
/// new devnet hash in the milestone log).
#[test]
fn devnet_genesis_hash_is_pinned() {
    let g = Genesis::from_json(include_str!("../../../genesis/devnet.json")).unwrap();
    assert_eq!(
        crate::genesis_hash(&g).unwrap(),
        "0x8aba54f8d0c38cc9e931bc86779e3e6b9b40f0dee747a09f115d854529468403"
            .parse::<B256>()
            .unwrap()
    );
    // Deterministic across runs (no dependence on the cache).
    assert_eq!(
        crate::genesis::genesis_state_root(&g).unwrap(),
        crate::genesis::genesis_state_root(&g).unwrap()
    );
}

#[test]
fn gas_of_registration_and_evidence() {
    let g = dev_genesis();
    let mut evm = Evm::from_genesis(&g);
    let owner = Address::repeat_byte(0x31);
    let sk = dev_key(40);
    let pk = sk.public_key();
    evm.fund(owner, bolt(100));
    let (ok, _, reg) = evm.tx_gas(
        owner,
        STAKING,
        bolt(64),
        IStakingManager::registerCall {
            pubkey: bytes(pk.as_slice()),
            pubkeyPoint: bytes(&pubkey_point(&pk).unwrap()),
            pop: bytes(&signature_point(&sk.proof_of_possession()).unwrap()),
            feeRecipient: owner,
        }
        .abi_encode(),
    );
    assert!(ok);
    let (a, b) = (
        vote_msg(g.config.chain_id, 0, 1, B256::repeat_byte(1)),
        vote_msg(g.config.chain_id, 0, 1, B256::repeat_byte(2)),
    );
    let data =
        crate::evidence::submit_evidence_calldata(8, &pk, &a, &sk.sign(&a), &b, &sk.sign(&b))
            .unwrap();
    let (ok, _, ev) = evm.tx_gas(owner, CONSENSUS, U256::ZERO, data.to_vec());
    assert!(ok);
    eprintln!("gas: register {reg}, submitEvidence {ev}");
    assert!(reg < 600_000 && ev < 900_000, "register {reg}, evidence {ev}");
}
