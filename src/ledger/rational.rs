// Arbitrary-precision rational arithmetic for reward calculations
//
// Copied from torsten-ledger/src/state/rewards.rs
//
// This implementation is CRITICAL for correctness. Do not use floating point
// or smaller integer types - mainnet reward calculations will overflow.

use num_bigint::BigInt;
use num_traits::{Signed, Zero};

/// Arbitrary-precision rational number matching Haskell's `Rational`.
///
/// Uses `num_bigint::BigInt` for exact arithmetic with no overflow risk.
/// All intermediate reward calculations produce exact results; `floor_u64()`
/// applies the single floor operation at the end, matching Haskell's
/// `rationalToCoinViaFloor`.
///
/// Previous implementation used i128 with BigInt fallback, but the fallback
/// saturated to i128::MAX when results didn't fit, silently producing wrong
/// answers for mainnet-scale values (~36T circulation denominator).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rat {
    pub n: BigInt,
    pub d: BigInt,
}

impl Rat {
    pub fn new(n: impl Into<BigInt>, d: impl Into<BigInt>) -> Self {
        let d = d.into();
        let n = n.into();
        if d.is_zero() {
            return Rat {
                n: BigInt::from(0),
                d: BigInt::from(1),
            };
        }
        let g = Self::bigint_gcd(&n, &d);
        let (n, d) = (&n / &g, &d / &g);
        // Normalize sign: denominator always positive
        if d < BigInt::from(0) {
            Rat { n: -n, d: -d }
        } else {
            Rat { n, d }
        }
    }

    fn bigint_gcd(a: &BigInt, b: &BigInt) -> BigInt {
        let (mut a, mut b) = (a.abs(), b.abs());
        while !b.is_zero() {
            let t = b.clone();
            b = &a % &t;
            a = t;
        }
        if a.is_zero() {
            BigInt::from(1)
        } else {
            a
        }
    }

    pub fn add(&self, other: &Rat) -> Rat {
        let n = &self.n * &other.d + &other.n * &self.d;
        let d = &self.d * &other.d;
        Rat::new(n, d)
    }

    pub fn sub(&self, other: &Rat) -> Rat {
        let n = &self.n * &other.d - &other.n * &self.d;
        let d = &self.d * &other.d;
        Rat::new(n, d)
    }

    pub fn mul(&self, other: &Rat) -> Rat {
        Rat::new(&self.n * &other.n, &self.d * &other.d)
    }

    pub fn div(&self, other: &Rat) -> Rat {
        if other.n.is_zero() {
            return Rat::new(0i128, 1i128);
        }
        Rat::new(&self.n * &other.d, &self.d * &other.n)
    }

    pub fn min_rat(&self, other: &Rat) -> Rat {
        // a/b <= c/d iff a*d <= c*b (when b, d > 0)
        if &self.n * &other.d <= &other.n * &self.d {
            self.clone()
        } else {
            other.clone()
        }
    }

    pub fn floor_u64(&self) -> u64 {
        if self.d.is_zero() || self.n <= BigInt::from(0) {
            return 0;
        }
        let result = &self.n / &self.d;
        // The result of floor(reward) must always fit in u64
        u64::try_from(result).unwrap_or_else(|_| {
            tracing::warn!("Rat::floor_u64 overflow — value exceeds u64::MAX, clamping");
            u64::MAX
        })
    }

    /// Helper: create from i128 values (convenience for the common case)
    pub fn from_i128(n: i128, d: i128) -> Self {
        Rat::new(BigInt::from(n), BigInt::from(d))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rat_basic_arithmetic() {
        let a = Rat::from_i128(1, 2); // 1/2
        let b = Rat::from_i128(1, 3); // 1/3

        // 1/2 + 1/3 = 5/6
        let sum = a.add(&b);
        assert_eq!(sum.n, BigInt::from(5));
        assert_eq!(sum.d, BigInt::from(6));

        // 1/2 * 1/3 = 1/6
        let product = a.mul(&b);
        assert_eq!(product.n, BigInt::from(1));
        assert_eq!(product.d, BigInt::from(6));
    }

    #[test]
    fn test_rat_floor() {
        let r = Rat::from_i128(7, 3); // 7/3 = 2.333...
        assert_eq!(r.floor_u64(), 2);

        let r2 = Rat::from_i128(-5, 2); // Negative
        assert_eq!(r2.floor_u64(), 0);
    }

    #[test]
    fn test_rat_normalization() {
        let r = Rat::from_i128(6, 9); // Should normalize to 2/3
        assert_eq!(r.n, BigInt::from(2));
        assert_eq!(r.d, BigInt::from(3));
    }
}
