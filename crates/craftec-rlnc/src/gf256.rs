//! Galois Field GF(2⁸) arithmetic for RLNC.
//!
//! This module implements the four fundamental field operations (add, multiply,
//! divide, inverse) and the vectorised fused-multiply-add used as the hot loop
//! in both encoding and decoding.
//!
//! # Field definition
//!
//! GF(2⁸) is the degree-8 extension of GF(2). Elements are represented as
//! bytes (`u8`); addition is bitwise XOR; multiplication is polynomial
//! multiplication modulo the **AES irreducible polynomial**:
//!
//! ```text
//! p(x) = x⁸ + x⁴ + x³ + x + 1   ≡  0x11B
//! ```
//!
//! # Lookup tables
//!
//! Multiplication via log/exp tables avoids the bit-manipulation loop entirely:
//!
//! ```text
//! a * b = EXP[ (LOG[a] + LOG[b]) mod 255 ]   (both non-zero)
//! ```
//!
//! `EXP_TABLE` has 512 entries (two copies of the 255-element cycle) so that
//! the modular reduction `(LOG[a] + LOG[b]) mod 255` can be replaced by a
//! single array lookup without a branch.

// ── Table generation ──────────────────────────────────────────────────────────

/// Multiply two GF(2⁸) elements using the "times-two" (xtime) approach.
///
/// This is used **only** during table initialisation and is not part of the
/// hot path.
#[inline(always)]
const fn gf_mul_slow(mut a: u8, mut b: u8) -> u8 {
    let mut result: u8 = 0;
    while b > 0 {
        if b & 1 != 0 {
            result ^= a;
        }
        // xtime: multiply a by x (shift left), reduce if degree-8 bit set
        let hi = a & 0x80;
        a <<= 1;
        if hi != 0 {
            a ^= 0x1B; // 0x11B mod 0x100 = 0x1B
        }
        b >>= 1;
    }
    result
}

/// Compute the EXP table: `EXP_TABLE[i] = g^i mod p(x)` where `g = 0x03`.
///
/// The table has 512 entries: the first 255 are one complete cycle of the
/// multiplicative group; the next 255 duplicate them so index arithmetic
/// never needs a modulo operation.
const fn build_exp_table() -> [u8; 512] {
    let mut table = [0u8; 512];
    let mut x: u8 = 1;
    let mut i = 0usize;
    while i < 255 {
        table[i] = x;
        table[i + 255] = x;
        // g = 3 = (x + 1), so next element = 3 * x in GF(2⁸)
        x = gf_mul_slow(x, 3);
        i += 1;
    }
    // The 510th entry wraps back to g^0 = 1; leave table[255] and table[510] = 1.
    table[255] = 1;
    table[510] = 1;
    table
}

/// Compute the LOG table: `LOG_TABLE[EXP_TABLE[i]] = i`.
///
/// `LOG_TABLE[0]` is undefined (log of zero is undefined in the field);
/// it is set to 0 and must never be used.
const fn build_log_table(exp: &[u8; 512]) -> [u8; 256] {
    let mut table = [0u8; 256];
    let mut i = 0usize;
    while i < 255 {
        table[exp[i] as usize] = i as u8;
        i += 1;
    }
    table
}

// Build both tables at compile time.
const EXP_INIT: [u8; 512] = build_exp_table();
const LOG_INIT: [u8; 256] = build_log_table(&EXP_INIT);

/// Exponential (anti-logarithm) table for GF(2⁸).
///
/// `EXP_TABLE[i]` = `g^i` where `g = 0x03` is a primitive element of GF(2⁸)
/// with respect to the AES irreducible polynomial `p(x) = x⁸+x⁴+x³+x+1`.
///
/// The table has 512 entries to allow index arithmetic without modulo:
/// `EXP_TABLE[i]` == `EXP_TABLE[i mod 255]` for all `i`.
pub static EXP_TABLE: [u8; 512] = EXP_INIT;

/// Logarithm table for GF(2⁸).
///
/// `LOG_TABLE[a]` = `log_g(a)` = the discrete logarithm of `a` with respect
/// to the primitive generator `g = 0x03`.
///
/// `LOG_TABLE[0]` is undefined and must not be used.
pub static LOG_TABLE: [u8; 256] = LOG_INIT;

// ── Field operations ─────────────────────────────────────────────────────────

/// Add two GF(2⁸) elements.
///
/// Addition in GF(2⁸) is bitwise XOR.
///
/// ```
/// # use craftec_rlnc::gf256::gf_add;
/// assert_eq!(gf_add(0b1010, 0b1100), 0b0110);
/// ```
#[inline(always)]
pub fn gf_add(a: u8, b: u8) -> u8 {
    a ^ b
}

/// Multiply two GF(2⁸) elements using log/exp lookup tables.
///
/// If either operand is zero the result is zero (by convention).
///
/// ```
/// # use craftec_rlnc::gf256::gf_mul;
/// // 1 is the multiplicative identity.
/// assert_eq!(gf_mul(7, 1), 7);
/// assert_eq!(gf_mul(0, 99), 0);
/// ```
#[inline(always)]
pub fn gf_mul(a: u8, b: u8) -> u8 {
    if a == 0 || b == 0 {
        return 0;
    }
    // SAFETY: LOG_TABLE indices are valid for all u8 values.
    // EXP_TABLE has 512 entries; log_a + log_b ≤ 254 + 254 = 508 < 512.
    let log_a = LOG_TABLE[a as usize] as usize;
    let log_b = LOG_TABLE[b as usize] as usize;
    EXP_TABLE[log_a + log_b]
}

/// Divide two GF(2⁸) elements: `a / b`.
///
/// Division by zero is undefined and will **panic** in debug builds.
/// In release builds the behaviour is unspecified (returns zero).
///
/// ```
/// # use craftec_rlnc::gf256::{gf_div, gf_mul};
/// let a = 57u8;
/// let b = 13u8;
/// let q = gf_div(a, b);
/// assert_eq!(gf_mul(q, b), a);
/// ```
#[inline(always)]
pub fn gf_div(a: u8, b: u8) -> u8 {
    debug_assert_ne!(b, 0, "GF(2^8) division by zero");
    if a == 0 {
        return 0;
    }
    let log_a = LOG_TABLE[a as usize] as usize;
    let log_b = LOG_TABLE[b as usize] as usize;
    // Add 255 before subtracting to avoid underflow; 255 mod 255 = 0 so it's safe.
    EXP_TABLE[log_a + 255 - log_b]
}

/// Compute the multiplicative inverse of a GF(2⁸) element.
///
/// `gf_inv(0)` is undefined and will **panic** in debug builds.
///
/// ```
/// # use craftec_rlnc::gf256::{gf_inv, gf_mul};
/// let a = 42u8;
/// assert_eq!(gf_mul(a, gf_inv(a)), 1);
/// ```
#[inline(always)]
pub fn gf_inv(a: u8) -> u8 {
    debug_assert_ne!(a, 0, "GF(2^8) inverse of zero");
    let log_a = LOG_TABLE[a as usize] as usize;
    // g^(255 - log_a) * g^log_a = g^255 = g^0 = 1
    EXP_TABLE[255 - log_a]
}

/// Fused-multiply-add over a byte slice: `dst[i] += coeff * src[i]` in GF(2⁸).
///
/// This is the **hot loop** for both encoding and Gaussian elimination.
/// The operation is identical to a SAXPY (scalar × vector + vector) in GF(2⁸).
///
/// When `coeff == 0` the function returns immediately (no-op).
/// When `coeff == 1` it degenerates to an XOR loop (still O(n) but no table
/// lookups).
///
/// The implementation is written in a simple scalar style that LLVM can
/// auto-vectorise to SIMD instructions with `-C target-cpu=native`.
///
/// # Panics
///
/// Panics in debug mode if `dst.len() != src.len()`.
#[inline]
pub fn gf_vec_mul_add(dst: &mut [u8], src: &[u8], coeff: u8) {
    debug_assert_eq!(dst.len(), src.len(), "gf_vec_mul_add: length mismatch");

    if coeff == 0 {
        return;
    }

    if coeff == 1 {
        // Fast path: addition only (XOR).
        for (d, &s) in dst.iter_mut().zip(src.iter()) {
            *d ^= s;
        }
        return;
    }

    // General path: multiply each source byte by coeff, then XOR into dst.
    let log_c = LOG_TABLE[coeff as usize] as usize;
    for (d, &s) in dst.iter_mut().zip(src.iter()) {
        if s != 0 {
            let log_s = LOG_TABLE[s as usize] as usize;
            *d ^= EXP_TABLE[log_c + log_s];
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── Table sanity ─────────────────────────────────────────────────────────

    #[test]
    fn exp_log_are_inverses() {
        // For every non-zero element a: EXP_TABLE[LOG_TABLE[a]] == a
        for a in 1u8..=255 {
            let log_a = LOG_TABLE[a as usize] as usize;
            assert_eq!(EXP_TABLE[log_a], a, "EXP/LOG mismatch for a = {a}");
        }
    }

    #[test]
    fn exp_table_is_cyclic() {
        // g^255 == g^0 == 1 (the group has order 255)
        assert_eq!(EXP_TABLE[0], 1);
        assert_eq!(EXP_TABLE[255], 1);
    }

    #[test]
    fn log_table_duplicate_half_matches() {
        // EXP_TABLE[i] == EXP_TABLE[i + 255] for i in 0..255
        for i in 0..255usize {
            assert_eq!(
                EXP_TABLE[i],
                EXP_TABLE[i + 255],
                "EXP_TABLE duplication failed at i = {i}"
            );
        }
    }

    // ── Addition properties ───────────────────────────────────────────────────

    #[test]
    fn add_commutativity() {
        for a in 0u8..=255 {
            for b in [0u8, 1, 7, 127, 200, 255] {
                assert_eq!(gf_add(a, b), gf_add(b, a));
            }
        }
    }

    #[test]
    fn add_associativity() {
        let (a, b, c) = (53u8, 97u8, 213u8);
        assert_eq!(gf_add(gf_add(a, b), c), gf_add(a, gf_add(b, c)));
    }

    #[test]
    fn add_identity_is_zero() {
        for a in 0u8..=255 {
            assert_eq!(gf_add(a, 0), a);
            assert_eq!(gf_add(0, a), a);
        }
    }

    #[test]
    fn add_self_is_zero() {
        for a in 0u8..=255 {
            assert_eq!(gf_add(a, a), 0);
        }
    }

    // ── Multiplication properties ─────────────────────────────────────────────

    #[test]
    fn mul_commutativity() {
        for a in 0u8..=255 {
            for b in [0u8, 1, 3, 7, 127, 255] {
                assert_eq!(gf_mul(a, b), gf_mul(b, a));
            }
        }
    }

    #[test]
    fn mul_associativity() {
        let (a, b, c) = (53u8, 97u8, 213u8);
        assert_eq!(gf_mul(gf_mul(a, b), c), gf_mul(a, gf_mul(b, c)));
    }

    #[test]
    fn mul_identity_is_one() {
        for a in 0u8..=255 {
            assert_eq!(gf_mul(a, 1), a);
            assert_eq!(gf_mul(1, a), a);
        }
    }

    #[test]
    fn mul_by_zero_is_zero() {
        for a in 0u8..=255 {
            assert_eq!(gf_mul(a, 0), 0);
            assert_eq!(gf_mul(0, a), 0);
        }
    }

    #[test]
    fn mul_distributivity_over_add() {
        // a * (b + c) == a*b + a*c
        let (a, b, c) = (123u8, 45u8, 67u8);
        let lhs = gf_mul(a, gf_add(b, c));
        let rhs = gf_add(gf_mul(a, b), gf_mul(a, c));
        assert_eq!(lhs, rhs);
    }

    #[test]
    fn mul_full_correctness_spot_check() {
        // Known GF(2^8) products (AES field, generator 0x03):
        // 0x53 * 0xCA = 0x01  (inverse pair)
        assert_eq!(gf_mul(0x53, 0xCA), 1);
        // 0x02 * 0x80 = 0x1B  (xtime of 0x80 with AES poly)
        assert_eq!(gf_mul(0x02, 0x80), 0x1B);
    }

    // ── Division / inverse properties ─────────────────────────────────────────

    #[test]
    fn div_is_mul_by_inverse() {
        for a in 0u8..=255 {
            for b in 1u8..=255 {
                assert_eq!(gf_div(a, b), gf_mul(a, gf_inv(b)));
            }
        }
    }

    #[test]
    fn inverse_identity() {
        // a * inv(a) == 1 for all a != 0
        for a in 1u8..=255 {
            assert_eq!(gf_mul(a, gf_inv(a)), 1, "a * inv(a) != 1 for a = {a}");
        }
    }

    #[test]
    fn inv_of_one_is_one() {
        assert_eq!(gf_inv(1), 1);
    }

    // ── gf_vec_mul_add ─────────────────────────────────────────────────────────

    #[test]
    fn vec_mul_add_zero_coeff_noop() {
        let src = vec![0xABu8; 32];
        let mut dst = vec![0x00u8; 32];
        gf_vec_mul_add(&mut dst, &src, 0);
        assert!(dst.iter().all(|&b| b == 0));
    }

    #[test]
    fn vec_mul_add_one_coeff_is_xor() {
        let src = vec![0xFFu8; 16];
        let mut dst = vec![0xFFu8; 16];
        gf_vec_mul_add(&mut dst, &src, 1);
        // dst XOR src with src == dst  →  all zeros
        assert!(dst.iter().all(|&b| b == 0));
    }

    #[test]
    fn vec_mul_add_correctness() {
        // dst[i] += coeff * src[i] should match element-wise gf_mul+gf_add.
        let src: Vec<u8> = (0..16).map(|i| (i * 17 + 3) as u8).collect();
        let original_dst: Vec<u8> = (0..16).map(|i| (i * 31 + 7) as u8).collect();
        let coeff = 0x5A_u8;

        let mut dst = original_dst.clone();
        gf_vec_mul_add(&mut dst, &src, coeff);

        for i in 0..16 {
            let expected = gf_add(original_dst[i], gf_mul(coeff, src[i]));
            assert_eq!(dst[i], expected, "mismatch at index {i}");
        }
    }

    #[test]
    fn vec_mul_add_large_buffer() {
        let n = 256 * 1024; // 256 KiB — typical piece size
        let src: Vec<u8> = (0..n).map(|i| (i % 251) as u8).collect();
        let mut dst = vec![0u8; n];
        gf_vec_mul_add(&mut dst, &src, 0x03);
        // Spot-check a few elements.
        for i in [0, 1, 127, 1000, n - 1] {
            let expected = gf_mul(0x03, src[i]);
            assert_eq!(dst[i], expected, "large buffer mismatch at index {i}");
        }
    }
}
