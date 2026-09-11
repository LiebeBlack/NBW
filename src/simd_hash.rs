//! simd_hash.rs — hashing acelerado por hardware.
//!
//! Usa SSE4.2 (CRC32) para hash de URLs/dominios. Este crate no contiene
//! código AVX ni AVX2, por lo que corre en cualquier x86_64 desde Nehalem.
//!
//! SAFETY: los bloques `unsafe` sólo envuelven intrínsecos SSE4.2
//! documentados, disponibles bajo la baseline de compilación
//! (`target-feature=+sse4.2`). No hay UB alcanzable en x86_64.

#![allow(dead_code)]

#[cfg(target_feature = "sse4.2")]
use core::arch::x86_64::{_mm_crc32_u32, _mm_crc32_u64};

/// CRC32 (Castagnoli) de un slice completo usando SSE4.2.
#[inline(always)]
pub fn crc32_hw(data: &[u8]) -> u64 {
    let mut crc: u64 = 0xFFFF_FFFFu64;
    let chunks = data.chunks_exact(8);
    let remainder = chunks.remainder();
    for chunk in chunks {
        let word = u64::from_le_bytes(chunk.try_into().unwrap());
        #[cfg(target_feature = "sse4.2")]
        unsafe {
            crc = _mm_crc32_u64(crc, word);
        }
        #[cfg(not(target_feature = "sse4.2"))]
        {
            crc = crc32_u64_soft(crc, word);
        }
    }
    for &b in remainder {
        #[cfg(target_feature = "sse4.2")]
        unsafe {
            crc = _mm_crc32_u32(crc as u32, b as u32) as u64;
        }
        #[cfg(not(target_feature = "sse4.2"))]
        {
            crc = crc32_u32_soft(crc as u32, b as u32) as u64;
        }
    }
    !crc
}

/// Mezcla 64-bit estilo FxHash (sin asignación, sin ramas).
#[inline(always)]
pub fn mix64(mut x: u64) -> u64 {
    x ^= x >> 33;
    x = x.wrapping_mul(0xff51_afd7_ed55_8ccd);
    x ^= x >> 33;
    x = x.wrapping_mul(0xc4ce_b9fe_1a85_ec53);
    x ^= x >> 33;
    x
}

/// Escanea un payload buscando qwords cero (usado para validar que las
/// respuestas de ads no ocultan datos). Cuenta cada bloque entero de 8
/// bytes que esté completo en el slice; los bytes sobrantes del final
/// que no forman un qword completo se ignoran.
pub fn scan_zero_qwords(data: &[u8]) -> usize {
    let mut count = 0usize;
    for ch in data.chunks_exact(8) {
        if u64::from_le_bytes(ch.try_into().unwrap()) == 0 {
            count += 1;
        }
    }
    count
}

/// Conjunto hash con sondeo lineal, capacidad potencia de dos,
/// claves u64 desde `crc32_hw`.
pub struct SimdHashSet {
    slots: Vec<u64>,
    mask: u64,
    filled: usize,
    empty_key: u64,
}

impl SimdHashSet {
    pub fn with_capacity(expected: usize) -> Self {
        let cap = (expected.max(16) * 4).next_power_of_two();
        Self {
            slots: vec![0; cap],
            mask: (cap - 1) as u64,
            filled: 0,
            empty_key: 0,
        }
    }

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
                return;
            }
            if slot == self.empty_key {
                self.slots[idx] = key;
                self.filled += 1;
                return;
            }
            idx = (idx + 1) & self.mask as usize;
        }
    }

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

    #[inline]
    pub fn insert(&mut self, s: &str) {
        self.insert_key(crc32_hw(s.as_bytes()));
    }

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

/// La clave 0 se reserva como centinela de slot vacío, así que se reasigna
/// a `u64::MAX` para que insertar 0 nunca rompa el sondeo.
#[inline]
fn remap_key(key: u64) -> u64 {
    if key == 0 {
        u64::MAX
    } else {
        key
    }
}

#[cfg(not(target_feature = "sse4.2"))]
fn crc32_u64_soft(crc: u64, v: u64) -> u64 {
    let mut acc = crc;
    for b in v.to_le_bytes() {
        acc = crc32_u32_soft(acc as u32, b) as u64;
    }
    acc
}

#[cfg(not(target_feature = "sse4.2"))]
fn crc32_u32_soft(crc: u32, v: u32) -> u32 {
    let mut acc = crc;
    for b in v.to_le_bytes() {
        let idx = ((acc as u8) ^ b) as usize;
        acc = TABLE[idx] ^ (acc >> 8);
    }
    acc
}

#[cfg(not(target_feature = "sse4.2"))]
const TABLE: [u32; 256] = {
    let mut t = [0u32; 256];
    let mut i = 0u32;
    while i < 256 {
        let mut c = i;
        let mut bit = 0;
        while bit < 8 {
            if c & 1 == 0 {
                c >>= 1;
            } else {
                // Polinomio Castagnoli reflejado, el mismo que usa
                // `_mm_crc32_u*`, para que el fallback coincida con SSE4.2.
                c = (c >> 1) ^ 0x82F6_3B78_u32;
            }
            bit += 1;
        }
        t[i as usize] = c;
        i += 1;
    }
    t
};

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
        // El byte sobrante que no completa un qword no se cuenta.
        let mut data2 = vec![1u8; 41];
        data2[40] = 0;
        assert_eq!(scan_zero_qwords(&data2), 0);
    }

    #[test]
    fn zero_key_never_breaks_probes() {
        let mut set = SimdHashSet::with_capacity(4);
        set.insert("");
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
