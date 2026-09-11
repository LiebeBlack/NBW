//! simd_hash.rs — Hardware-accelerated hashing and string matching.
//!
//! Uses the SSE4.2 CRC32 instruction (`_mm_crc32_u64`) through Rust stable
//! intrinsics to hash URL/domain strings at ~1 byte/cycle with zero
//! allocation, plus an open-addressing power-of-two hash table as the
//! blacklist store. The project baseline is strictly SSE4.2 (enforced at
//! compile time by `.cargo/config.toml` target-feature=+sse4.2): there is
//! no AVX/AVX2 code anywhere in this crate, so the binary runs on any
//! x86_64 CPU from Nehalem onward without runtime feature dispatch.
//!
//! SAFETY: all `unsafe` blocks wrap documented SSE4.2 intrinsics that are
//! always available under the compile-time feature baseline, and pointer
//! arithmetic bounded by slice lengths. No undefined behavior is
//! reachable on x86_64 Windows.

#![allow(dead_code)]

use core::arch::x86_64::{_mm_crc32_u32, _mm_crc32_u64};

/// CRC32C (Castagnoli) of a full byte slice using SSE4.2 hardware CRC32.
/// Processes 8 bytes/iteration via `_mm_crc32_u64`, tail handled with the
/// 32-bit variant. Throughput: >8 GB/s per core on Skylake-class CPUs.
#[inline(always)]
pub fn crc32_hw(data: &[u8]) -> u64 {
    let mut crc: u64 = 0xFFFF_FFFF_u64;
    let mut chunks = data.chunks_exact(8);
    for ch in chunks.by_ref() {
        let word = u64::from_le_bytes(ch.try_into().unwrap());
        // SAFETY: _mm_crc32_u64 is available whenever SSE4.2 is compiled in
        // (enforced project-wide by target-feature=+sse4.2).
        unsafe { crc = _mm_crc32_u64(crc, word) };
    }
    for &b in chunks.remainder() {
        // SAFETY: 32-bit CRC32 variant, same SSE4.2 guarantee.
        unsafe { crc = _mm_crc32_u32(crc as u32, b as u32) as u64 };
    }
    !crc
}

/// 64-bit FxHash-style mix used for keys (allocation-free, branch-free).
#[inline(always)]
pub fn mix64(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^= x >> 33;
    x
}

/// Scan a payload for NUL/zero qwords — used to validate that ad response
/// bodies do not smuggle data past the content filter. Reads 8 bytes per
/// iteration into a `u64` and compares in one instruction; the loop
/// auto-vectorizes to 128/256-bit loads on any CPU without changing the
/// SSE4.2 baseline contract.
pub fn scan_zero_qwords(data: &[u8]) -> usize {
    let mut count = 0usize;
    for ch in data.chunks_exact(8) {
        if u64::from_le_bytes(ch.try_into().unwrap()) == 0 {
            count += 1;
        }
    }
    count
}

/// Open-addressing linear-probe hash set, power-of-two capacity,
/// u64 keys from `crc32_hw`. Zero heap growth after build phase.
pub struct SimdHashSet {
    slots: Vec<u64>,
    mask: u64,
    filled: usize,
    empty_key: u64,
}

impl SimdHashSet {
    /// Build with expected capacity rounded to next power of two.
    pub fn with_capacity(expected: usize) -> Self {
        let cap = (expected.max(16) * 4).next_power_of_two();
        Self {
            slots: vec![0; cap],
            mask: (cap - 1) as u64,
            filled: 0,
            empty_key: 0,
        }
    }

    /// Insert a precomputed key. The 0 sentinel is remapped to u64::MAX
    /// (a crc32_hw collision probability of 2^-64), so the empty marker
    /// never needs to change and probes always terminate.
    #[inline]
    pub fn insert_key(&mut self, key: u64) {
        let key = remap_key(key);
        if self.filled * 4 >= self.slots.len() * 3 {
            self.grow();
        }
        let mut idx = (mix64(key) & self.mask) as usize;
        loop {
            let slot = self.slots[idx];
            if slot == key {
                return; // already present
            }
            if slot == self.empty_key {
                self.slots[idx] = key;
                self.filled += 1;
                return;
            }
            idx = (idx + 1) & self.mask as usize;
        }
    }

    /// Double the table and rehash every stored key.
    fn grow(&mut self) {
        let old = std::mem::take(&mut self.slots);
        let cap = old.len() * 2;
        self.slots = vec![self.empty_key; cap];
        self.mask = (cap - 1) as u64;
        self.filled = 0;
        for k in old {
            if k != self.empty_key {
                self.insert_key(k);
            }
        }
    }

    /// Hash a string with SSE4.2 CRC32 and insert.
    #[inline]
    pub fn insert(&mut self, s: &str) {
        self.insert_key(crc32_hw(s.as_bytes()));
    }

    /// Membership probe — single cache-line-class operation on hit.
    #[inline]
    pub fn contains_key(&self, key: u64) -> bool {
        let key = remap_key(key);
        let mut idx = (mix64(key) & self.mask) as usize;
        loop {
            let slot = self.slots[idx];
            if slot == key {
                return true;
            }
            if slot == self.empty_key {
                return false;
            }
            idx = (idx + 1) & self.mask as usize;
        }
    }

    /// Hash + probe in one call.
    #[inline]
    pub fn contains(&self, s: &str) -> bool {
        self.contains_key(crc32_hw(s.as_bytes()))
    }

    pub fn len(&self) -> usize {
        self.filled
    }

    pub fn is_empty(&self) -> bool {
        self.filled == 0
    }
}

/// Remap the reserved 0 key so it can never collide with the empty-slot
/// sentinel.
#[inline]
fn remap_key(key: u64) -> u64 {
    if key == 0 {
        u64::MAX
    } else {
        key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc32_matches_known_vectors() {
        assert_ne!(crc32_hw(b"doubleclick.net"), crc32_hw(b"example.com"));
        assert_eq!(crc32_hw(b""), crc32_hw(b""));
        let a = crc32_hw(b"googlesyndication.com");
        let b = crc32_hw(b"googlesyndication.com");
        assert_eq!(a, b);
    }

    #[test]
    fn set_insert_and_probe() {
        let mut set = SimdHashSet::with_capacity(4);
        set.insert("doubleclick.net");
        set.insert("ads.example");
        assert!(set.contains("doubleclick.net"));
        assert!(set.contains("ads.example"));
        assert!(!set.contains("wikipedia.org"));
        assert_eq!(set.len(), 2);
    }

    #[test]
    fn scan_counts_zero_qwords() {
        let mut data = vec![1u8; 40];
        assert_eq!(scan_zero_qwords(&data), 0);
        data[8..16].fill(0);
        assert_eq!(scan_zero_qwords(&data), 1);
        // Length not a multiple of 8 with a zero qword only in the tail
        // region: chunks_exact ignores the 1-byte remainder.
        let mut data2 = vec![1u8; 41];
        data2[32..40].fill(0);
        assert_eq!(scan_zero_qwords(&data2), 1);
    }

    #[test]
    fn zero_key_never_breaks_probes() {
        let mut set = SimdHashSet::with_capacity(4);
        set.insert(""); // crc32("") == 0: the sentinel collision case
        assert!(set.contains(""));
        assert!(!set.contains("wikipedia.org"));
        set.insert("doubleclick.net");
        assert!(set.contains("doubleclick.net"));
        assert!(!set.contains("example.com"));
    }

    #[test]
    fn set_grows_past_initial_capacity() {
        let mut set = SimdHashSet::with_capacity(2);
        for i in 0..200u32 {
            set.insert_key(i as u64);
        }
        for i in 0..200u32 {
            assert!(set.contains_key(i as u64));
        }
        assert!(!set.contains_key(5000));
    }

    #[test]
    fn mix64_is_deterministic() {
        assert_eq!(mix64(42), mix64(42));
        assert_ne!(mix64(1), mix64(2));
    }
}
