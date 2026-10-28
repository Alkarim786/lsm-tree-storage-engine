//! Bloom filter using double hashing (Kirsch-Mitzenmacher), as in LevelDB.

pub fn hash(key: &[u8]) -> u64 {
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for &b in key {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h ^= h >> 33;
    h = h.wrapping_mul(0xff51_afd7_ed55_8ccd);
    h ^= h >> 33;
    h
}

#[derive(Debug, Clone)]
pub struct Bloom {
    bits: Vec<u8>,
    k: u32,
}

impl Bloom {
    pub fn disabled() -> Self {
        Bloom {
            bits: Vec::new(),
            k: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.bits.is_empty()
    }

    pub fn from_hashes(hashes: &[u64], bits_per_key: usize) -> Self {
        if bits_per_key == 0 {
            return Self::disabled();
        }
        let nbytes = (hashes.len() * bits_per_key).max(64).div_ceil(8);
        let nbits = (nbytes * 8) as u64;
        let k = ((bits_per_key as f64 * 0.69) as u32).clamp(1, 30);
        let mut bits = vec![0u8; nbytes];
        for &h0 in hashes {
            let mut h = h0;
            let delta = h0.rotate_right(17) | 1;
            for _ in 0..k {
                let pos = (h % nbits) as usize;
                bits[pos / 8] |= 1 << (pos % 8);
                h = h.wrapping_add(delta);
            }
        }
        Bloom { bits, k }
    }

    pub fn may_contain(&self, key: &[u8]) -> bool {
        if self.bits.is_empty() {
            return true;
        }
        let nbits = (self.bits.len() * 8) as u64;
        let h0 = hash(key);
        let mut h = h0;
        let delta = h0.rotate_right(17) | 1;
        for _ in 0..self.k {
            let pos = (h % nbits) as usize;
            if self.bits[pos / 8] & (1 << (pos % 8)) == 0 {
                return false;
            }
            h = h.wrapping_add(delta);
        }
        true
    }

    pub fn encode(&self) -> Vec<u8> {
        if self.bits.is_empty() {
            return Vec::new();
        }
        let mut v = self.bits.clone();
        v.push(self.k as u8);
        v
    }

    pub fn decode(data: &[u8]) -> Self {
        match data.split_last() {
            None => Self::disabled(),
            Some((&k, bits)) => Bloom {
                bits: bits.to_vec(),
                k: k as u32,
            },
        }
    }
}

/// Textbook false-positive probability (1 - e^(-kn/m))^k.
pub fn theoretical_fp_rate(n: usize, m_bits: usize, k: u32) -> f64 {
    (1.0 - (-((k as f64) * n as f64) / m_bits as f64).exp()).powi(k as i32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_false_negatives_and_bounded_fp_rate() {
        let n = 10_000;
        let keys: Vec<Vec<u8>> = (0..n).map(|i| format!("key{i}").into_bytes()).collect();
        let hashes: Vec<u64> = keys.iter().map(|k| hash(k)).collect();
        let b = Bloom::from_hashes(&hashes, 10);
        assert!(keys.iter().all(|k| b.may_contain(k)));
        let trials = 100_000;
        let fp = (0..trials)
            .filter(|i| b.may_contain(format!("miss{i}").as_bytes()))
            .count();
        let rate = fp as f64 / trials as f64;
        let theory = theoretical_fp_rate(n, n * 10, 6);
        assert!(rate < 0.02, "fp rate {rate} too high (theory {theory})");
        assert!(rate < theory * 2.0, "measured {rate} vs theory {theory}");
    }

    #[test]
    fn encode_roundtrip() {
        let hashes: Vec<u64> = (0..100u32).map(|i| hash(&i.to_le_bytes())).collect();
        let b = Bloom::from_hashes(&hashes, 10);
        let d = Bloom::decode(&b.encode());
        for i in 0..100u32 {
            assert!(d.may_contain(&i.to_le_bytes()));
        }
        assert!(Bloom::decode(&[]).may_contain(b"anything"));
    }
}
