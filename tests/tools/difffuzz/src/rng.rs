//! std だけの擬似乱数（xorshift64*、シードは splitmix64 で撹拌）。同じシードなら常に同じ列を返す。

pub struct Rng(u64);

fn splitmix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

impl Rng {
    /// (seed, case) から独立した乱数列を作る。`--case` で 1 ケースだけ再現できる。
    pub fn for_case(seed: u64, case: u64) -> Self {
        let s = splitmix(splitmix(seed) ^ case.wrapping_mul(0xD6E8_FEB8_6659_FD93));
        Rng(if s == 0 { 0x1234_5678_9ABC_DEF1 } else { s })
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// 0..n（n=0 のときは 0）。
    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }

    /// lo..=hi
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        lo + self.below((hi - lo + 1) as u64) as i64
    }

    /// pct パーセントの確率で true。
    pub fn chance(&mut self, pct: u64) -> bool {
        self.below(100) < pct
    }

    pub fn pick<'a, T>(&mut self, xs: &'a [T]) -> &'a T {
        &xs[self.below(xs.len() as u64) as usize]
    }

    /// 重み付き選択。`weights[i]` の比で添字を返す。
    pub fn weighted(&mut self, weights: &[u64]) -> usize {
        let total: u64 = weights.iter().sum();
        let mut r = self.below(total);
        for (i, w) in weights.iter().enumerate() {
            if r < *w {
                return i;
            }
            r -= *w;
        }
        weights.len() - 1
    }
}
