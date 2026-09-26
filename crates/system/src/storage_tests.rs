//! Storage providers without stake (ADR 0012): free signed registration, rewards that build a
//! bond, and a burned bond share plus deactivation on a failed audit.

use crate::{abi::*, addresses::*, tests::Evm};
use alloy_primitives::{Address, B256, Bytes, U256};
use alloy_signer::SignerSync;
use alloy_signer_local::PrivateKeySigner;

const BASE: u32 = bolt_primitives::params::STORAGE_ID_BASE;

fn bolt(n: u64) -> U256 {
    U256::from(n) * U256::from(10u128.pow(18))
}

fn evm() -> Evm {
    let mut evm = Evm::from_genesis(&crate::tests::dev_genesis());
    evm.timestamp = evm.timestamp.max(1_000_000);
    evm
}

/// Registers `who` through a relayer from its signature; returns the id.
fn register(evm: &mut Evm, who: &PrivateKeySigner, peer: &[u8]) -> u32 {
    let deadline = U256::from(evm.timestamp + 3600);
    let peer = Bytes::copy_from_slice(peer);
    let digest: B256 = evm.view(
        HISTORY,
        IHistoryRegistry::registrationDigestCall {
            account: who.address(),
            peerId: peer.clone(),
            deadline,
        },
    );
    let sig = Bytes::from(who.sign_hash_sync(&digest).unwrap().as_bytes().to_vec());
    let relayer = Address::repeat_byte(0xa1);
    evm.fund(relayer, bolt(1));
    let call = IHistoryRegistry::registerStorageForCall {
        account: who.address(),
        peerId: peer,
        deadline,
        sig,
    };
    let id = evm.call(relayer, HISTORY, U256::ZERO, call.clone());
    assert!(!evm.try_call(relayer, HISTORY, U256::ZERO, call), "a signature registers once");
    id
}

/// One audit task on `provider` in `epoch`, decided `ok`.
fn audit(evm: &mut Evm, epoch: u64, provider: u32, ok: bool) {
    evm.system(
        HISTORY,
        IHistoryRegistry::beginAuditsCall {
            epoch,
            panel: vec![1],
            providers: vec![provider],
            targets: vec![0],
            heights: vec![1],
        },
    );
    assert!(evm.system(HISTORY, IHistoryRegistry::recordAuditCall { epoch, task: 0, ok }));
}

#[test]
fn a_provider_without_bolt_registers_earns_and_builds_a_bond() {
    let mut evm = evm();
    let who = PrivateKeySigner::random();
    assert_eq!(evm.balance(who.address()), U256::ZERO);
    let id = register(&mut evm, &who, b"peer-1");
    assert_eq!(id, BASE);
    assert_eq!(evm.view(HISTORY, IHistoryRegistry::activeStorageProvidersCall {}), vec![BASE]);
    assert_eq!(
        evm.view(HISTORY, IHistoryRegistry::peerOfCall { id }),
        Bytes::from_static(b"peer-1")
    );
    assert_eq!(
        evm.view(HISTORY, IHistoryRegistry::storageIdsOfCall { account: who.address() }),
        vec![id]
    );

    // A passed audit earns the epoch's storage reward (here 14,400 BOLT for the only pass).
    audit(&mut evm, 5, id, true);
    let emission = bolt(14_400);
    let paid = evm.view(REWARDS, IRewardDistributor::previewStorageCall { epoch: 5, emission });
    assert_eq!(paid, emission);
    evm.fund(REWARDS, paid);
    let supply = evm.view(REWARDS, IRewardDistributor::supplyCall {});
    evm.system(REWARDS, IRewardDistributor::settleStorageCall { epoch: 5, emission });
    assert_eq!(evm.view(REWARDS, IRewardDistributor::supplyCall {}), supply + emission);

    // Anyone claims for it: 20% goes to the bond, capped at 64 BOLT; the rest to the account.
    evm.call(Address::repeat_byte(0xa1), REWARDS, U256::ZERO, IRewardDistributor::claimCall { id });
    let p = evm.view(HISTORY, IHistoryRegistry::storageProviderCall { id });
    assert_eq!(p.bond, bolt(64));
    assert!(p.active);
    assert_eq!(evm.balance(who.address()), emission - bolt(64));
}

#[test]
fn a_failed_audit_burns_part_of_the_bond_and_deactivates() {
    let mut evm = evm();
    let who = PrivateKeySigner::random();
    let id = register(&mut evm, &who, b"peer-1");
    audit(&mut evm, 5, id, true);
    evm.fund(REWARDS, bolt(100));
    evm.system(REWARDS, IRewardDistributor::settleStorageCall { epoch: 5, emission: bolt(100) });
    evm.call(who.address(), REWARDS, U256::ZERO, IRewardDistributor::claimCall { id });
    assert_eq!(evm.view(HISTORY, IHistoryRegistry::storageProviderCall { id }).bond, bolt(20));

    let supply = evm.view(REWARDS, IRewardDistributor::supplyCall {});
    audit(&mut evm, 6, id, false);
    let p = evm.view(HISTORY, IHistoryRegistry::storageProviderCall { id });
    assert_eq!(p.bond, bolt(18), "10% burned");
    assert!(!p.active, "no more assignments");
    assert!(evm.view(HISTORY, IHistoryRegistry::activeStorageProvidersCall {}).is_empty());
    assert_eq!(evm.view(REWARDS, IRewardDistributor::supplyCall {}), supply - bolt(2));

    // The rest of the bond comes back after the exit delay.
    let before = evm.balance(who.address());
    assert!(!evm.try_call(
        who.address(),
        HISTORY,
        U256::ZERO,
        IHistoryRegistry::releaseStorageBondCall { id }
    ));
    evm.timestamp += 14 * 86_400;
    evm.call(who.address(), HISTORY, U256::ZERO, IHistoryRegistry::releaseStorageBondCall { id });
    assert_eq!(evm.balance(who.address()) - before, bolt(18));
}

#[test]
fn only_the_account_moves_or_exits_its_provider() {
    let mut evm = evm();
    let who = PrivateKeySigner::random();
    let id = register(&mut evm, &who, b"peer-1");
    let other = Address::repeat_byte(0x77);
    evm.fund(other, bolt(1));
    evm.fund(who.address(), bolt(1));
    let set = IHistoryRegistry::setStoragePeerCall { id, peerId: Bytes::from_static(b"peer-2") };
    assert!(!evm.try_call(other, HISTORY, U256::ZERO, set.clone()));
    evm.call(who.address(), HISTORY, U256::ZERO, set);
    assert!(!evm.try_call(other, HISTORY, U256::ZERO, IHistoryRegistry::exitStorageCall { id }));
    evm.call(who.address(), HISTORY, U256::ZERO, IHistoryRegistry::exitStorageCall { id });
    assert!(!evm.view(HISTORY, IHistoryRegistry::storageProviderCall { id }).active);
    // Earnings enter only through RewardDistributor (which keeps the bond accounting honest).
    assert!(!evm.try_call(other, HISTORY, bolt(1), IHistoryRegistry::depositEarningsCall { id }));
}
