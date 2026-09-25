//! Relative ASERT difficulty adjustment (after BCH's aserti3-2d).
//!
//! `next = parent_difficulty · 2^(−(Δt − T) / τ)` where `Δt` is the parent's solve time (parent
//! timestamp minus grandparent timestamp), `T` the target spacing and `τ` the half-life. Blocks
//! on schedule keep the difficulty; a solve time of `T + τ` halves it; `T − τ` doubles it.
//! The exponential uses aserti3-2d's integer cubic approximation of `2^x` on 16 fractional bits.
//! Timestamps strictly increase (checked elsewhere), so `Δt ≥ 1`.

use alloy_primitives::U256;

/// Parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AsertParams {
    /// Target block spacing in seconds.
    pub spacing: u64,
    /// Half-life in seconds.
    pub half_life: u64,
    /// Difficulty of blocks 1 and 2 (before two timestamps exist to measure a solve time).
    pub initial: U256,
    /// Lower bound.
    pub minimum: U256,
}

/// Difficulty of the child of a block with `parent_difficulty` mined `solve_time` seconds after
/// its own parent. `parent_number` 0 or 1 gives `params.initial`.
pub fn next_difficulty(
    params: &AsertParams,
    parent_number: u64,
    parent_difficulty: U256,
    solve_time: u64,
) -> U256 {
    if parent_number <= 1 {
        return params.initial.max(params.minimum);
    }
    // exponent in 1/65536 units: (Δt − T) · 65536 / τ, positive when blocks are slow.
    let num = (solve_time as i128 - params.spacing as i128) * 65_536;
    let exponent = num.div_euclid(params.half_life.max(1) as i128);
    let shifts = exponent.div_euclid(65_536);
    let frac = exponent.rem_euclid(65_536) as u128;
    // 2^(frac/65536) · 65536, aserti3-2d polynomial.
    let factor = 65_536u128
        + ((195_766_423_245_049u128 * frac
            + 971_821_376u128 * frac * frac
            + 5_127u128 * frac * frac * frac
            + (1u128 << 47))
            >> 48);
    // difficulty scales with 2^(−exponent): divide by factor, shift right by `shifts`.
    let mut d = parent_difficulty.saturating_mul(U256::from(65_536u64)) / U256::from(factor);
    if shifts >= 0 {
        d = if shifts >= 256 { U256::ZERO } else { d >> (shifts as usize) };
    } else {
        let s = (-shifts) as usize;
        d = if s >= 256 || d.leading_zeros() < s { U256::MAX } else { d << s };
    }
    d.max(params.minimum)
}
