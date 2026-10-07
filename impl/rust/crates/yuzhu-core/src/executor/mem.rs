//! クエリ単位のメモリ予算（`m4/00-contracts.md` §10、D-19、`m4/05` §8）。
//!
//! 溜める系ノード（Sort・Materialize・HashAggregate・HashJoin の build 側・CTE など）が行を溜めるたびに
//! `charge` し、作り直し・破棄の前に `release` する。スピルはしない。上限を超えたら `53200`。

use std::cell::Cell;

use yuzhu_numeric::Numeric;

use crate::error::{Error, Result, sqlstate};
use crate::types::{Datum, Row};

/// `yuzhu.query_mem_limit` の既定値（256MB）。
pub const DEFAULT_QUERY_MEM_LIMIT: usize = 256 << 20;

/// 行の `Vec` のヘッダとアロケータの余白。
pub const ROW_OVERHEAD: usize = 32;
/// ハッシュ表のスロット・バケットの添字・印。
pub const HASH_ENTRY_OVERHEAD: usize = 48;

#[derive(Debug)]
pub struct MemBudget {
    limit: usize,
    used: Cell<usize>,
    peak: Cell<usize>,
}

impl MemBudget {
    /// 無制限は `usize::MAX`。
    pub fn new(limit: usize) -> Self {
        MemBudget {
            limit,
            used: Cell::new(0),
            peak: Cell::new(0),
        }
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// `bytes` を予算に加える。`used + bytes > limit` なら課金せずに `53200 out of memory`
    /// （DETAIL に要求量・現在の使用量・`yuzhu.query_mem_limit`）。失敗した文は中断される。
    pub fn charge(&self, bytes: usize) -> Result<()> {
        let used = self.used.get();
        let next = used.saturating_add(bytes);
        if next > self.limit {
            return Err(Error::new(sqlstate::OUT_OF_MEMORY, "out of memory")
                .with_detail(format!(
                    "Failed on request of {bytes} bytes: the query already holds {used} bytes and yuzhu.query_mem_limit is {} bytes.",
                    self.limit
                ))
                .with_hint("Increase yuzhu.query_mem_limit or reduce the amount of data the query holds in memory."));
        }
        self.used.set(next);
        if next > self.peak.get() {
            self.peak.set(next);
        }
        Ok(())
    }

    /// 予算から `bytes` を返す（使用量を下回らない）。
    pub fn release(&self, bytes: usize) {
        self.used.set(self.used.get().saturating_sub(bytes));
    }

    pub fn used(&self) -> usize {
        self.used.get()
    }

    /// これまでの使用量の最大値（EXPLAIN ANALYZE の末尾の情報に使ってよい）。
    pub fn peak(&self) -> usize {
        self.peak.get()
    }
}

/// `numeric` の base-10000 の桁数（NaN・無限大は 0）。
fn numeric_ndigits(n: &Numeric) -> usize {
    match n {
        Numeric::Finite(f) => f.digits().len(),
        Numeric::NaN | Numeric::PosInf | Numeric::NegInf => 0,
    }
}

/// `numeric` 1 つの概算のバイト数（`Numeric` 自身 + 桁。`Datum` の分は含まない）。
pub fn estimate_numeric_bytes(n: &Numeric) -> usize {
    std::mem::size_of::<Numeric>() + 2 * numeric_ndigits(n)
}

/// 値 1 つの概算のバイト数（`Datum` 自身 + ヒープ上の中身）。長さ（容量ではない）で数えるので、
/// 同じ値は同じ見積りになる。`Datum` の変種を足したらここもコンパイルエラーになり、見積りの漏れに気づける。
pub fn estimate_datum_bytes(d: &Datum) -> usize {
    let heap = match d {
        Datum::Text(s) | Datum::BpChar(s) => s.len(),
        Datum::Numeric(n) => estimate_numeric_bytes(n),
        Datum::Int2Vector(v) => 2 * v.len(),
        Datum::OidVector(v) => 4 * v.len(),
        Datum::Int4Array(v) => 8 * v.len(),
        Datum::Null
        | Datum::Bool(_)
        | Datum::Int2(_)
        | Datum::Int4(_)
        | Datum::Int8(_)
        | Datum::Float4(_)
        | Datum::Float8(_)
        | Datum::Oid(_)
        | Datum::Char(_)
        | Datum::Xid(_)
        | Datum::Cid(_)
        | Datum::Tid(_)
        | Datum::Void
        | Datum::Date(_)
        | Datum::Timestamp(_)
        | Datum::TimestampTz(_) => 0,
    };
    std::mem::size_of::<Datum>() + heap
}

/// 行 1 つの概算のバイト数（`ROW_OVERHEAD` + 各値）。
pub fn estimate_row_bytes(row: &Row) -> usize {
    ROW_OVERHEAD + row.iter().map(estimate_datum_bytes).sum::<usize>()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn charge_release_and_limit() {
        let m = MemBudget::new(100);
        m.charge(60).unwrap();
        assert_eq!(m.used(), 60);
        let e = m.charge(41).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::OUT_OF_MEMORY);
        assert_eq!(e.message, "out of memory");
        assert_eq!(
            e.detail.as_deref(),
            Some(
                "Failed on request of 41 bytes: the query already holds 60 bytes and yuzhu.query_mem_limit is 100 bytes."
            )
        );
        assert!(e.hint.as_deref().unwrap().contains("yuzhu.query_mem_limit"));
        // 失敗した分は加わらない。
        assert_eq!(m.used(), 60);
        m.charge(40).unwrap();
        assert_eq!((m.used(), m.peak()), (100, 100));
        m.release(1000);
        assert_eq!(m.used(), 0);
        // ピークは下がらない。
        assert_eq!(m.peak(), 100);
        assert_eq!(m.limit(), 100);
        // usize のあふれは飽和して超過として扱う。
        assert!(m.charge(usize::MAX).is_err());
    }

    #[test]
    fn exactly_at_limit_succeeds_one_over_fails() {
        let m = MemBudget::new(10);
        m.charge(10).unwrap();
        assert!(m.charge(1).is_err());
        let m = MemBudget::new(10);
        assert!(m.charge(11).is_err());
        assert_eq!(m.used(), 0);
        // 無制限。
        let m = MemBudget::new(usize::MAX);
        m.charge(usize::MAX - 1).unwrap();
        m.charge(1).unwrap();
        assert_eq!(m.used(), usize::MAX);
    }

    #[test]
    fn row_estimate_grows_with_payload() {
        let small = estimate_row_bytes(&vec![Datum::Int4(1)]);
        let big = estimate_row_bytes(&vec![Datum::Int4(1), Datum::Text("x".repeat(100))]);
        assert!(big >= small + 100);
        assert_eq!(DEFAULT_QUERY_MEM_LIMIT, 256 << 20);
        assert_eq!(
            estimate_row_bytes(&vec![]),
            ROW_OVERHEAD,
            "空の行は ROW_OVERHEAD だけ"
        );
    }

    #[test]
    fn datum_estimates_are_deterministic_and_by_length() {
        let base = std::mem::size_of::<Datum>();
        assert_eq!(estimate_datum_bytes(&Datum::Null), base);
        assert_eq!(estimate_datum_bytes(&Datum::Text("abc".into())), base + 3);
        assert_eq!(estimate_datum_bytes(&Datum::BpChar("ab ".into())), base + 3);
        assert_eq!(
            estimate_datum_bytes(&Datum::OidVector(vec![1, 2])),
            base + 8
        );
        assert_eq!(
            estimate_datum_bytes(&Datum::Int2Vector(vec![1, 2])),
            base + 4
        );
        assert_eq!(
            estimate_datum_bytes(&Datum::Int4Array(vec![Some(1), None])),
            base + 16
        );
        // 容量ではなく長さ。
        let mut s = String::with_capacity(1000);
        s.push('a');
        assert_eq!(estimate_datum_bytes(&Datum::Text(s)), base + 1);
        let n = |t: &str| Datum::Numeric(Numeric::parse(t).unwrap());
        let one = estimate_datum_bytes(&n("1"));
        let big = estimate_datum_bytes(&n("123456789012345678901234567890"));
        assert!(big > one);
        assert_eq!(
            estimate_datum_bytes(&n("NaN")),
            base + std::mem::size_of::<Numeric>()
        );
    }
}
