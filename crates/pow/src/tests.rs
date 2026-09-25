use super::*;
use alloy_primitives::U256;

#[test]
fn seed_heights_rotate_with_lag() {
    assert_eq!(seed_height(0), 0);
    assert_eq!(seed_height(2048 + 64), 0);
    assert_eq!(seed_height(2048 + 65), 2048);
    assert_eq!(seed_height(4096 + 64), 2048);
    assert_eq!(seed_height(4096 + 65), 4096);
}

#[test]
fn hashes_are_deterministic_and_differ_from_monero_randomx() {
    let h = Hasher::new(Mode::Light);
    let key = key_for(&B256::repeat_byte(1));
    let a = h.hash(&key, b"boltchain").unwrap();
    assert_eq!(a, h.hash(&key, b"boltchain").unwrap());
    assert_ne!(a, h.hash(&key, b"boltchaim").unwrap());
    assert_ne!(a, h.hash(&key_for(&B256::repeat_byte(2)), b"boltchain").unwrap());
    // RandomX reference vector (Monero parameters): key "test key 000", input "This is a test"
    // hashes to 639183aae1bf4c9a35884cb46b09cad9175f04efd7684e7262a0ac1c2f0b4e3f. With the
    // Boltchain salt the same inputs must hash differently.
    let monero = "639183aae1bf4c9a35884cb46b09cad9175f04efd7684e7262a0ac1c2f0b4e3f";
    let raw_key = B256::right_padding_from(b"test key 000");
    let ours = h.hash(&raw_key, b"This is a test").unwrap();
    assert_ne!(format!("{ours:x}").trim_start_matches("0x"), monero);
}

#[test]
fn difficulty_check() {
    let pow = B256::with_last_byte(1);
    assert!(meets(&pow, U256::from(1u64)));
    assert!(meets(&B256::repeat_byte(0xff), U256::from(1u64)));
    assert!(!meets(&B256::repeat_byte(0xff), U256::from(2u64)));
    let half = B256::from(U256::MAX / U256::from(2u64));
    assert!(meets(&half, U256::from(2u64)));
    assert!(!meets(&pow, U256::ZERO));
}

fn params() -> AsertParams {
    AsertParams {
        spacing: 12,
        half_life: 3_600,
        initial: U256::from(1_000_000u64),
        minimum: U256::from(1u64),
    }
}

#[test]
fn asert_keeps_on_schedule_halves_and_doubles() {
    let p = params();
    let d = U256::from(1_000_000u64);
    assert_eq!(next_difficulty(&p, 0, d, 12), p.initial);
    assert_eq!(next_difficulty(&p, 5, d, 12), d, "on schedule: unchanged");
    let halved = next_difficulty(&p, 5, d, 12 + 3_600);
    assert_eq!(halved, d / U256::from(2u64));
    let quick = next_difficulty(&p, 5, d, 1); // 11 s early: slightly harder
    assert!(quick > d && quick < d * U256::from(101u64) / U256::from(100u64));
    // Consistently slow blocks (24 s instead of 12) for one half-life's worth of excess time halve it.
    let mut x = d;
    for _ in 0..300 {
        x = next_difficulty(&p, 5, x, 24);
    }
    let ratio = x.to::<u128>() as f64 / 500_000.0;
    assert!((ratio - 1.0).abs() < 0.01, "after 300 slow blocks: {x}");
    // never below the minimum
    assert_eq!(next_difficulty(&p, 5, U256::from(1u64), 100_000), U256::from(1u64));
}

#[test]
fn block_reward_matches_daily_emission() {
    // 7,200 blocks, each paying from the shrinking pool, issue one day's emission (2,038,588 BOLT
    // on day one, as in the plan).
    let mut unissued = bolt_cap();
    for _ in 0..7_200 {
        unissued -= block_reward(unissued, 10_000);
    }
    let day = (bolt_cap() - unissued) / U256::from(10u128.pow(18));
    assert!((2_038_550..=2_038_620).contains(&day.to::<u64>()), "{day}");
}

fn bolt_cap() -> U256 {
    U256::from(1u64 << 32) * U256::from(10u128.pow(18))
}

#[test]
fn search_finds_a_valid_nonce() {
    for algo in [Algorithm::RandomBolt, Algorithm::Keccak] {
        let pow = Pow::light(algo);
        let key = key_for(&B256::repeat_byte(3));
        let seal = B256::repeat_byte(9);
        let stop = std::sync::atomic::AtomicBool::new(false);
        let (nonce, hash) =
            pow.search(&key, &seal, U256::from(8u64), 0, 1, 1_000, &stop).unwrap().expect("found");
        assert_eq!(pow.hash(&key, &seal, nonce).unwrap(), hash);
        assert!(meets(&hash, U256::from(8u64)));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(pow.search(&key, &seal, U256::MAX, 0, 1, 1_000, &stop).unwrap().is_none());
    }
}

#[test]
fn header_seal_verifies() {
    let pow = Pow::light(Algorithm::Keccak);
    let key = key_for(&B256::repeat_byte(1));
    let mut h = Header { difficulty: U256::from(1000u64), number: 5, ..Default::default() };
    let stop = std::sync::atomic::AtomicBool::new(false);
    let (nonce, _) =
        pow.search(&key, &seal_hash(&h), h.difficulty, 0, 1, 1_000_000, &stop).unwrap().unwrap();
    h.nonce = B64::from(nonce);
    assert!(pow.verify(&key, &h).unwrap());
    // Deterministic inputs: the next nonce happens not to meet the target.
    h.nonce = B64::from(nonce + 1);
    assert!(!pow.verify(&key, &h).unwrap());
    h.nonce = B64::from(nonce);
    h.difficulty = U256::MAX;
    assert!(!pow.verify(&key, &h).unwrap());
}
