//! FNV-1a hashing, used for state fingerprints and lockstep desync checks.

pub const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
pub const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

#[inline]
pub fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h = FNV_OFFSET;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

#[inline]
pub fn fnv1a64_u64(h: &mut u64, v: u64) {
    for b in v.to_le_bytes() {
        *h ^= b as u64;
        *h = h.wrapping_mul(FNV_PRIME);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_vector() {
        // FNV-1a 64 of the empty string and of "a".
        assert_eq!(fnv1a64(b""), FNV_OFFSET);
        assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
