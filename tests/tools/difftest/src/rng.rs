//! Small deterministic PRNG (`xoshiro256**`, seeded with `SplitMix64`).
//!
//! The generator is hand-written so that a seed reproduces exactly the same
//! SQL regardless of the version of any external crate.

#[derive(Clone, Debug)]
pub(crate) struct Rng {
    s: [u64; 4],
}

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        let mut x = seed;
        let mut next = || {
            x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
            let mut z = x;
            z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
            z ^ (z >> 31)
        };
        Self {
            s: [next(), next(), next(), next()],
        }
    }

    /// An independent stream for `(seed, stream)`, e.g. one per round.
    pub(crate) fn derive(seed: u64, stream: u64) -> Self {
        let mut base = Self::new(seed);
        let k = base.next_u64();
        Self::new(k ^ stream.wrapping_mul(0xD6E8_FEB8_6659_FD93).rotate_left(17))
    }

    pub(crate) fn next_u64(&mut self) -> u64 {
        let result = self.s[1].wrapping_mul(5).rotate_left(7).wrapping_mul(9);
        let t = self.s[1] << 17;
        self.s[2] ^= self.s[0];
        self.s[3] ^= self.s[1];
        self.s[1] ^= self.s[2];
        self.s[0] ^= self.s[3];
        self.s[2] ^= t;
        self.s[3] = self.s[3].rotate_left(45);
        result
    }

    /// Uniform in `0..n` (`n > 0`).
    pub(crate) fn below(&mut self, n: usize) -> usize {
        assert!(n > 0, "below(0)");
        let n64 = u64::try_from(n).expect("usize fits u64");
        usize::try_from(self.next_u64() % n64).expect("value < n fits usize")
    }

    /// Uniform in `lo..=hi`.
    pub(crate) fn range(&mut self, lo: i64, hi: i64) -> i64 {
        assert!(lo <= hi);
        let span = u128::try_from(i128::from(hi) - i128::from(lo) + 1).expect("positive span");
        let v = u128::from(self.next_u64()) % span;
        i64::try_from(i128::from(lo) + i128::try_from(v).expect("fits")).expect("in range")
    }

    /// True with probability `num / den`.
    pub(crate) fn chance(&mut self, num: u32, den: u32) -> bool {
        self.below(den as usize) < num as usize
    }

    pub(crate) fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len())]
    }

    /// Index chosen with the given relative weights.
    pub(crate) fn weighted(&mut self, weights: &[u32]) -> usize {
        let total: u32 = weights.iter().sum();
        let mut x = u32::try_from(self.below(total as usize)).expect("fits");
        for (i, w) in weights.iter().enumerate() {
            if x < *w {
                return i;
            }
            x -= w;
        }
        weights.len() - 1
    }

    pub(crate) fn shuffle<T>(&mut self, xs: &mut [T]) {
        for i in (1..xs.len()).rev() {
            let j = self.below(i + 1);
            xs.swap(i, j);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Rng;

    #[test]
    fn deterministic() {
        let mut a = Rng::derive(42, 7);
        let mut b = Rng::derive(42, 7);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        let mut c = Rng::new(1);
        for _ in 0..1000 {
            let v = c.range(-3, 3);
            assert!((-3..=3).contains(&v));
            assert!(c.below(5) < 5);
        }
        assert!((i64::MIN..=i64::MAX).contains(&c.range(i64::MIN, i64::MAX)));
    }
}
