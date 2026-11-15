//! Deterministic PRNG (xorshift64*).
//!
//! The core must behave identically on every machine and every platform so
//! that lockstep netplay stays in sync: no `std::time`, no hardware entropy,
//! only explicit seeds. `u64` arithmetic wraps identically everywhere.

#[derive(Clone, Debug)]
pub struct Rng {
    state: u64,
}

impl Rng {
    pub fn new(seed: u64) -> Rng {
        // Avoid the all-zero fixed point.
        Rng {
            state: seed ^ 0x9E37_79B9_7F4A_7C15,
        }
    }

    #[inline]
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform-ish value below `n`. Good enough for tie-breaks; `n` is tiny
    /// here so modulo bias is negligible and, more importantly, it is
    /// deterministic.
    #[inline]
    pub fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            return 0;
        }
        (self.next_u64() % (n as u64)) as usize
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic() {
        let mut a = Rng::new(42);
        let mut b = Rng::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        let mut c = Rng::new(43);
        assert_ne!(a.next_u64(), c.next_u64());
    }

    #[test]
    fn zero_seed_is_usable() {
        let mut r = Rng::new(0);
        assert_ne!(r.next_u64(), 0);
        assert_ne!(r.next_u64(), 0);
    }
}
