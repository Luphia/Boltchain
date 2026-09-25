use crate::{
    BlockInput,
    block::{BlockExecutor, BlockParams, TxRejection, next_base_fee},
};
use alloy_consensus::{Header, SignableTransaction, TxEip1559, TxEnvelope};
use alloy_eips::{
    eip2935::{HISTORY_STORAGE_ADDRESS, HISTORY_STORAGE_CODE},
    eip4788::{BEACON_ROOTS_ADDRESS, BEACON_ROOTS_CODE},
};
use alloy_network::TxSignerSync;
use alloy_primitives::{Address, B256, Bytes, TxKind, U256};
use alloy_signer_local::PrivateKeySigner;
use bolt_primitives::params::{CHAIN_ID, DEFAULT_GAS_LIMIT, MIN_BASE_FEE_WEI};
use revm::{DatabaseRef, bytecode::Bytecode, database::InMemoryDB, state::AccountInfo};

const ONE_ETH: u128 = 1_000_000_000_000_000_000;

fn setup() -> (InMemoryDB, PrivateKeySigner) {
    let signer = PrivateKeySigner::random();
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        signer.address(),
        AccountInfo { balance: U256::from(10 * ONE_ETH), ..Default::default() },
    );
    for (a, c) in [
        (BEACON_ROOTS_ADDRESS, &BEACON_ROOTS_CODE),
        (HISTORY_STORAGE_ADDRESS, &HISTORY_STORAGE_CODE),
    ] {
        db.insert_account_info(
            a,
            AccountInfo { nonce: 1, ..Default::default() }.with_code(Bytecode::new_raw(c.clone())),
        );
    }
    (db, signer)
}

fn params(number: u64) -> BlockParams {
    BlockParams {
        input: BlockInput {
            chain_id: CHAIN_ID,
            number,
            timestamp: 1_800_000_000 + number * 6,
            beneficiary: Address::repeat_byte(0xbe),
            gas_limit: DEFAULT_GAS_LIMIT,
            base_fee: MIN_BASE_FEE_WEI,
            prevrandao: B256::repeat_byte(7),
        },
        parent_hash: B256::repeat_byte(0x11),
        parent_beacon_root: B256::ZERO,
        extra_data: Bytes::new(),
        difficulty: alloy_primitives::U256::ZERO,
        nonce: alloy_primitives::B64::ZERO,
    }
}

fn transfer(
    signer: &PrivateKeySigner,
    nonce: u64,
    to: Address,
    value: u128,
    gas: u64,
) -> TxEnvelope {
    let mut tx = TxEip1559 {
        chain_id: CHAIN_ID,
        nonce,
        gas_limit: gas,
        max_fee_per_gas: 10 * u128::from(MIN_BASE_FEE_WEI),
        max_priority_fee_per_gas: u128::from(MIN_BASE_FEE_WEI),
        to: TxKind::Call(to),
        value: U256::from(value),
        ..Default::default()
    };
    let sig = signer.sign_transaction_sync(&mut tx).unwrap();
    tx.into_signed(sig).into()
}

#[test]
fn transfer_block_and_header() {
    let (db, signer) = setup();
    let bob = Address::repeat_byte(0xb0);
    let mut ex = BlockExecutor::new(&db, params(1)).unwrap();
    let r = ex.add(transfer(&signer, 0, bob, ONE_ETH, 21_000), signer.address()).unwrap().unwrap();
    assert!(r.status());
    assert_eq!(r.cumulative_gas_used(), 21_000);
    let block = ex.finish();
    assert_eq!(block.gas_used, 21_000);
    let bob_acc = block.bundle.account(&bob).unwrap().info.clone().unwrap();
    assert_eq!(bob_acc.balance, U256::from(ONE_ETH));

    // EIP-2935: the parent hash is stored at slot (number - 1) % 8191.
    let h = block.bundle.account(&HISTORY_STORAGE_ADDRESS).unwrap();
    assert_eq!(h.storage_slot(U256::ZERO), Some(U256::from_be_bytes(B256::repeat_byte(0x11).0)));

    let header = block.header(B256::repeat_byte(0x55));
    assert_eq!(header.gas_used, 21_000);
    assert_ne!(header.transactions_root, alloy_consensus::constants::EMPTY_ROOT_HASH);
    assert_eq!(header.blob_gas_used, Some(0));
    assert_eq!(header.number, 1);
}

#[test]
fn rejected_transactions_leave_state_untouched() {
    let (db, signer) = setup();
    let bob = Address::repeat_byte(0xb0);
    let mut ex = BlockExecutor::new(&db, params(1)).unwrap();
    // Wrong nonce.
    let err = ex.add(transfer(&signer, 5, bob, 1, 21_000), signer.address()).unwrap().unwrap_err();
    assert!(matches!(err, TxRejection::Invalid(_)), "{err:?}");
    // Gas limit above what is left in the block.
    let err = ex
        .add(transfer(&signer, 0, bob, 1, DEFAULT_GAS_LIMIT + 1), signer.address())
        .unwrap()
        .unwrap_err();
    assert!(matches!(err, TxRejection::BlockGasExhausted { .. }), "{err:?}");
    assert_eq!(ex.tx_count(), 0);
    let block = ex.finish();
    assert!(block.bundle.account(&bob).is_none());
    assert_eq!(db.basic_ref(signer.address()).unwrap().unwrap().nonce, 0);
}

#[test]
fn many_transfers_fill_the_block() {
    let (db, signer) = setup();
    let mut ex = BlockExecutor::new(&db, params(1)).unwrap();
    let mut n = 0u64;
    loop {
        let to = Address::with_last_byte((n % 200) as u8);
        match ex.add(transfer(&signer, n, to, 1, 21_000), signer.address()).unwrap() {
            Ok(_) => n += 1,
            Err(TxRejection::BlockGasExhausted { .. }) => break,
            Err(e) => panic!("{e:?}"),
        }
    }
    assert_eq!(n, DEFAULT_GAS_LIMIT / 21_000);
}

#[test]
fn base_fee_follows_eip1559_with_floor() {
    let full = Header {
        gas_used: 30_000_000,
        gas_limit: 30_000_000,
        base_fee_per_gas: Some(1_000_000_000),
        ..Default::default()
    };
    assert_eq!(next_base_fee(&full, MIN_BASE_FEE_WEI), 1_125_000_000);
    let empty = Header {
        gas_used: 0,
        gas_limit: 30_000_000,
        base_fee_per_gas: Some(MIN_BASE_FEE_WEI),
        ..Default::default()
    };
    assert_eq!(next_base_fee(&empty, MIN_BASE_FEE_WEI), MIN_BASE_FEE_WEI);
}
