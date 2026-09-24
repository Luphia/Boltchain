use super::*;
use revm::{
    bytecode::{Bytecode, opcode},
    context::result::{ExecutionResult, InvalidTransaction, Output},
    context_interface::result::EVMError,
    database::InMemoryDB,
    precompile::Precompiles,
    primitives::{Bytes, TxKind, address, eip7825::TX_GAS_LIMIT_CAP},
    state::AccountInfo,
};

const CALLER: Address = address!("00000000000000000000000000000000000c0001");
const CONTRACT: Address = address!("00000000000000000000000000000000000c0002");

fn block() -> BlockInput {
    BlockInput {
        number: 1,
        timestamp: 1_800_000_006,
        beneficiary: Address::repeat_byte(0xbe),
        gas_limit: bolt_primitives::params::DEFAULT_GAS_LIMIT,
        base_fee: bolt_primitives::params::MIN_BASE_FEE_WEI,
        prevrandao: B256::repeat_byte(0x42),
    }
}

/// A database with a funded caller and `code` deployed at `CONTRACT`.
fn db_with(code: Vec<u8>) -> InMemoryDB {
    let mut db = InMemoryDB::default();
    db.insert_account_info(
        CALLER,
        AccountInfo { balance: U256::from(10u128.pow(21)), ..Default::default() },
    );
    db.insert_account_info(
        CONTRACT,
        AccountInfo::default().with_code(Bytecode::new_legacy(Bytes::from(code))),
    );
    db
}

fn call(gas_limit: u64) -> TxEnv {
    TxEnv::builder()
        .caller(CALLER)
        .kind(TxKind::Call(CONTRACT))
        .gas_limit(gas_limit)
        .gas_price(u128::from(bolt_primitives::params::MIN_BASE_FEE_WEI))
        .chain_id(Some(CHAIN_ID))
        .build()
        .unwrap()
}

/// Runs `code` and returns the 32-byte word it returns.
fn run_and_return_word(code: Vec<u8>) -> U256 {
    let out = transact(db_with(code), &block(), call(100_000)).unwrap();
    match out.result {
        ExecutionResult::Success { output: Output::Call(bytes), .. } => U256::from_be_slice(&bytes),
        other => panic!("unexpected result: {other:?}"),
    }
}

/// `<push value> ; MSTORE at 0 ; RETURN 32 bytes`
fn return_top(mut code: Vec<u8>) -> Vec<u8> {
    code.extend([opcode::PUSH0, opcode::MSTORE, 0x60, 0x20, opcode::PUSH0, opcode::RETURN]);
    code
}

#[test]
fn clz_opcode_is_enabled() {
    // CLZ(1) = 255 (EIP-7939)
    let code = return_top(vec![0x60, 0x01, opcode::CLZ]);
    assert_eq!(run_and_return_word(code), U256::from(255));
    // CLZ(0) = 256
    let code = return_top(vec![opcode::PUSH0, opcode::CLZ]);
    assert_eq!(run_and_return_word(code), U256::from(256));
}

#[test]
fn chainid_is_8017() {
    let code = return_top(vec![opcode::CHAINID]);
    assert_eq!(run_and_return_word(code), U256::from(8017));
}

#[test]
fn osaka_precompile_set() {
    let p = Precompiles::osaka();
    let p256 = address!("0000000000000000000000000000000000000100");
    assert!(p.contains(&p256), "EIP-7951 P256VERIFY must be active");
    // EIP-2537 BLS12-381 precompiles (0x0b..=0x11) are needed for on-chain QC checks.
    for i in 0x0bu8..=0x11 {
        assert!(p.contains(&Address::with_last_byte(i)), "BLS precompile {i:#x} missing");
    }
    // KZG point evaluation (0x0a) stays callable for EVM equivalence even without blob txs.
    assert!(p.contains(&Address::with_last_byte(0x0a)));
}

#[test]
fn tx_gas_limit_cap_eip7825() {
    let err =
        transact(db_with(vec![opcode::STOP]), &block(), call(TX_GAS_LIMIT_CAP + 1)).unwrap_err();
    assert!(
        matches!(
            err,
            ExecError::Evm(EVMError::Transaction(
                InvalidTransaction::TxGasLimitGreaterThanCap { .. }
            ))
        ),
        "got {err:?}"
    );
    assert!(transact(db_with(vec![opcode::STOP]), &block(), call(TX_GAS_LIMIT_CAP)).is_ok());
}

#[test]
fn blob_transactions_are_rejected() {
    let mut tx = call(100_000);
    tx.tx_type = BLOB_TX_TYPE;
    tx.blob_hashes = vec![B256::repeat_byte(1)];
    let err = transact(db_with(vec![opcode::STOP]), &block(), tx).unwrap_err();
    assert!(matches!(err, ExecError::BlobTxRejected));
}

#[test]
fn wrong_chain_id_is_rejected() {
    let mut tx = call(100_000);
    tx.chain_id = Some(1);
    assert!(transact(db_with(vec![opcode::STOP]), &block(), tx).is_err());
}
