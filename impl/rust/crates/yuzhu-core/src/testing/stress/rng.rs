//! xoshiro256**（`tests/tools/difftest` と同じ生成器）。外部クレートの版に左右されない。

/// 再現できる乱数。`seed` から splitmix64 で状態を作る。
#[derive(Clone, Debug)]
pub struct Xoshiro {
    s: [u64; 4],
}

fn splitmix64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl Xoshiro {
    pub fn new(seed: u64) -> Self {
        let mut sm = seed;
        let s = [
            splitmix64(&mut sm),
            splitmix64(&mut sm),
            splitmix64(&mut sm),
            splitmix64(&mut sm),
        ];
        Xoshiro { s }
    }

    /// スレッド `tid` 用の乱数（同じ `seed` と `tid` なら同じ列）。
    pub fn for_thread(seed: u64, tid: usize) -> Self {
        Xoshiro::new(seed ^ (tid as u64 + 1).wrapping_mul(0xD1B5_4A32_D192_ED03))
    }

    pub fn next_u64(&mut self) -> u64 {
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

    /// `0..n`（`n` が 0 なら 0）。
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            return 0;
        }
        // 剰余の偏りは試験の乱数として許容する。
        self.next_u64() % n
    }

    /// `lo..=hi`。
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        debug_assert!(lo <= hi);
        let span = u64::try_from(hi - lo).unwrap_or(0) + 1;
        lo + i64::try_from(self.below(span)).unwrap_or(0)
    }

    /// 確率 `percent`%（0〜100）で true。
    pub fn chance(&mut self, percent: u64) -> bool {
        self.below(100) < percent
    }

    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[usize::try_from(self.below(items.len() as u64)).unwrap_or(0)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_sequence() {
        let mut a = Xoshiro::new(42);
        let mut b = Xoshiro::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
        let mut c = Xoshiro::new(43);
        assert_ne!(Xoshiro::new(42).next_u64(), c.next_u64());
    }

    #[test]
    fn threads_differ() {
        assert_ne!(
            Xoshiro::for_thread(1, 0).next_u64(),
            Xoshiro::for_thread(1, 1).next_u64()
        );
    }

    #[test]
    fn bounds() {
        let mut r = Xoshiro::new(7);
        let mut seen = [false; 5];
        for _ in 0..1000 {
            let v = r.range(-2, 2);
            assert!((-2..=2).contains(&v));
            seen[usize::try_from(v + 2).unwrap()] = true;
            assert!(r.below(3) < 3);
        }
        assert!(seen.iter().all(|s| *s));
        assert_eq!(r.below(0), 0);
        assert!(!r.chance(0));
        assert!(r.chance(100));
    }
}
