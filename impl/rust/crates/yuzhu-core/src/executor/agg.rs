//! 集約の部品（`m4/05` §3.4・§6、11 §7.1 の C-4）。
//!
//! [`AggState`] は集約 1 つの遷移状態、[`AggSet`] は `PhysAgg` の並びの評価の定義、[`AggGroup`] は
//! グループ 1 つの状態。`Aggregate` / `HashAggregate` / `GroupAggregate` が共有する（D5-10）。
//!
//! 1 入力行の流し方（§6.1）: FILTER → 引数の評価 → NULL を含めば飛ばす（M4 の集約はすべて引数について
//! STRICT。`count(*)` は行を数える）→ DISTINCT の既出を飛ばす → 遷移。

#![allow(clippy::doc_markdown, clippy::trivially_copy_pass_by_ref)]

use std::cmp::Ordering;
use std::collections::HashSet;

use yuzhu_numeric::Numeric;

use super::mem::{
    HASH_ENTRY_OVERHEAD, estimate_datum_bytes, estimate_numeric_bytes, estimate_row_bytes,
};
use super::{ExecCtx, eval};
use crate::catalog::AggKind;
use crate::error::{Error, Result, sqlstate};
use crate::planner::physical::{PhysAgg, PhysExpr, SortKey};
use crate::types::hash::HashKey;
use crate::types::{Datum, Row, cmp_datum};

/// 集約 1 つの遷移状態（§3.4）。
#[derive(Debug, Clone)]
pub enum AggState {
    CountStar(i64),
    Count(i64),
    SumInt2(Option<i64>),
    SumInt4(Option<i64>),
    SumInt8(Option<i128>),
    SumFloat4(Option<f32>),
    SumFloat8(Option<f64>),
    SumNumeric(Option<Numeric>),
    /// `avg(int2 / int4 / int8)`。
    AvgInt {
        count: i64,
        sum: i128,
    },
    /// `avg(float4 / float8)`。
    AvgFloat {
        count: i64,
        sum: f64,
        /// PG の `float8_accum` の Sxx（オーバーフロー検出のためだけに維持する）。
        sxx: f64,
    },
    AvgNumeric {
        count: i64,
        sum: Numeric,
    },
    /// `min` / `max`。
    MinMax {
        max: bool,
        cur: Option<Datum>,
    },
    BoolAnd(Option<bool>),
    BoolOr(Option<bool>),
    /// `string_agg`。まだ 1 件も無ければ `None`。
    StringAgg(Option<String>),
}

fn bigint_out_of_range() -> Error {
    Error::new(sqlstate::NUMERIC_VALUE_OUT_OF_RANGE, "bigint out of range")
}

fn float_overflow() -> Error {
    Error::new(
        sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
        "value out of range: overflow",
    )
}

fn bad_arg(what: &str) -> Error {
    Error::internal(format!("aggregate {what}: unexpected argument"))
}

/// 非 NULL の第 1 引数。
fn first<'a>(args: &'a [Datum], what: &str) -> Result<&'a Datum> {
    args.first().ok_or_else(|| bad_arg(what))
}

fn int_arg(args: &[Datum], what: &str) -> Result<i64> {
    first(args, what)?.as_i64().ok_or_else(|| bad_arg(what))
}

fn numeric_arg<'a>(args: &'a [Datum], what: &str) -> Result<&'a Numeric> {
    match first(args, what)? {
        Datum::Numeric(n) => Ok(n),
        _ => Err(bad_arg(what)),
    }
}

fn bool_arg(args: &[Datum], what: &str) -> Result<bool> {
    match first(args, what)? {
        Datum::Bool(b) => Ok(*b),
        _ => Err(bad_arg(what)),
    }
}

/// 整数（`i128` と `i64`）を小数点なしの numeric にする。
fn numeric_of_i128(v: i128) -> Result<Numeric> {
    Ok(Numeric::parse(&v.to_string())?)
}

/// 浮動小数の加算の結果が無限大で、2 つの入力はどちらも有限なら 22003（PostgreSQL の `check_float8_val`）。
fn add_f64(a: f64, b: f64) -> Result<f64> {
    let r = a + b;
    if r.is_infinite() && a.is_finite() && b.is_finite() {
        return Err(float_overflow());
    }
    Ok(r)
}

fn add_f32(a: f32, b: f32) -> Result<f32> {
    let r = a + b;
    if r.is_infinite() && a.is_finite() && b.is_finite() {
        return Err(float_overflow());
    }
    Ok(r)
}

impl AggState {
    /// 初期値の状態。
    pub fn new(kind: AggKind) -> AggState {
        match kind {
            AggKind::CountStar => AggState::CountStar(0),
            AggKind::Count => AggState::Count(0),
            AggKind::SumInt2 => AggState::SumInt2(None),
            AggKind::SumInt4 => AggState::SumInt4(None),
            AggKind::SumInt8 => AggState::SumInt8(None),
            AggKind::SumFloat4 => AggState::SumFloat4(None),
            AggKind::SumFloat8 => AggState::SumFloat8(None),
            AggKind::SumNumeric => AggState::SumNumeric(None),
            AggKind::AvgInt2 | AggKind::AvgInt4 | AggKind::AvgInt8 => {
                AggState::AvgInt { count: 0, sum: 0 }
            }
            AggKind::AvgFloat4 | AggKind::AvgFloat8 => AggState::AvgFloat {
                count: 0,
                sum: 0.0,
                sxx: 0.0,
            },
            AggKind::AvgNumeric => AggState::AvgNumeric {
                count: 0,
                sum: Numeric::zero(),
            },
            AggKind::Min => AggState::MinMax {
                max: false,
                cur: None,
            },
            AggKind::Max => AggState::MinMax {
                max: true,
                cur: None,
            },
            AggKind::BoolAnd => AggState::BoolAnd(None),
            AggKind::BoolOr => AggState::BoolOr(None),
            AggKind::StringAgg => AggState::StringAgg(None),
        }
    }

    /// 非 NULL の引数列（`count(*)` は空）で 1 回遷移する。NULL・FILTER・DISTINCT の判定は呼び出し側が
    /// 済ませている。
    #[allow(clippy::too_many_lines)]
    pub fn transition(&mut self, args: &[Datum]) -> Result<()> {
        match self {
            AggState::CountStar(n) | AggState::Count(n) => {
                *n = n.checked_add(1).ok_or_else(bigint_out_of_range)?;
            }
            AggState::SumInt2(s) | AggState::SumInt4(s) => {
                let v = int_arg(args, "sum")?;
                *s = Some(match *s {
                    None => v,
                    Some(cur) => cur.checked_add(v).ok_or_else(bigint_out_of_range)?,
                });
            }
            AggState::SumInt8(s) => {
                let v = i128::from(int_arg(args, "sum")?);
                // 2^64 行以上が必要なのでオーバーフローしない。
                *s = Some(s.unwrap_or(0) + v);
            }
            AggState::SumFloat4(s) => {
                let Datum::Float4(v) = first(args, "sum")? else {
                    return Err(bad_arg("sum"));
                };
                *s = Some(match *s {
                    None => *v,
                    Some(cur) => add_f32(cur, *v)?,
                });
            }
            AggState::SumFloat8(s) => {
                let Datum::Float8(v) = first(args, "sum")? else {
                    return Err(bad_arg("sum"));
                };
                *s = Some(match *s {
                    None => *v,
                    Some(cur) => add_f64(cur, *v)?,
                });
            }
            AggState::SumNumeric(s) => {
                let v = numeric_arg(args, "sum")?;
                *s = Some(match s.take() {
                    None => v.clone(),
                    Some(cur) => cur.checked_add(v)?,
                });
            }
            AggState::AvgInt { count, sum } => {
                let v = int_arg(args, "avg")?;
                *count += 1;
                *sum += i128::from(v);
            }
            AggState::AvgFloat { count, sum, sxx } => {
                let v = first(args, "avg")?.as_f64().ok_or_else(|| bad_arg("avg"))?;
                let new_sum = add_f64(*sum, v)?;
                // PG の float8_accum: Sxx を累積し、有限入力でオーバーフローしたらエラー。
                let n = *count + 1;
                #[allow(clippy::cast_precision_loss)]
                let nf = n as f64;
                let new_sxx = if *count == 0 {
                    if v.is_finite() { 0.0 } else { f64::NAN }
                } else if sum.is_infinite() || sum.is_nan() {
                    f64::NAN
                } else {
                    let tmp = v * nf - *sum;
                    *sxx + tmp * tmp / (nf * (nf - 1.0))
                };
                if new_sxx.is_infinite() && sum.is_finite() && v.is_finite() {
                    return Err(float_overflow());
                }
                *sum = new_sum;
                *sxx = new_sxx;
                *count = n;
            }
            AggState::AvgNumeric { count, sum } => {
                let v = numeric_arg(args, "avg")?;
                *sum = sum.checked_add(v)?;
                *count += 1;
            }
            AggState::MinMax { max, cur } => {
                let v = first(args, "min/max")?;
                // 等しいときも置き換える（後に来た値が代表。PostgreSQL の `*_larger` / `*_smaller`。11 §7.1 の C-4）。
                let replace = match cur {
                    None => true,
                    Some(c) => {
                        let ord = cmp_datum(v, c);
                        if *max {
                            ord != Ordering::Less
                        } else {
                            ord != Ordering::Greater
                        }
                    }
                };
                if replace {
                    *cur = Some(v.clone());
                }
            }
            AggState::BoolAnd(s) => {
                let v = bool_arg(args, "bool_and")?;
                *s = Some(s.map_or(v, |c| c && v));
            }
            AggState::BoolOr(s) => {
                let v = bool_arg(args, "bool_or")?;
                *s = Some(s.map_or(v, |c| c || v));
            }
            AggState::StringAgg(s) => {
                let Datum::Text(v) = first(args, "string_agg")? else {
                    return Err(bad_arg("string_agg"));
                };
                let delim = match args.get(1) {
                    Some(Datum::Text(d)) => d.as_str(),
                    _ => "",
                };
                match s {
                    None => *s = Some(v.clone()),
                    Some(cur) => {
                        cur.push_str(delim);
                        cur.push_str(v);
                    }
                }
            }
        }
        Ok(())
    }

    /// 結果（非消費。`rewind` での再利用のため）。非 NULL の入力が 0 件なら、`count` は 0、ほかは NULL。
    pub fn result(&self) -> Result<Datum> {
        Ok(match self {
            AggState::CountStar(n) | AggState::Count(n) => Datum::Int8(*n),
            AggState::SumInt2(s) | AggState::SumInt4(s) => s.map_or(Datum::Null, Datum::Int8),
            AggState::SumInt8(s) => match s {
                Some(v) => Datum::Numeric(numeric_of_i128(*v)?),
                None => Datum::Null,
            },
            AggState::SumFloat4(s) => s.map_or(Datum::Null, Datum::Float4),
            AggState::SumFloat8(s) => s.map_or(Datum::Null, Datum::Float8),
            AggState::SumNumeric(s) => s.clone().map_or(Datum::Null, Datum::Numeric),
            AggState::AvgInt { count, sum } => {
                if *count == 0 {
                    Datum::Null
                } else {
                    let n = numeric_of_i128(*sum)?;
                    let c = numeric_of_i128(i128::from(*count))?;
                    Datum::Numeric(n.checked_div(&c)?)
                }
            }
            AggState::AvgFloat { count, sum, .. } => {
                if *count == 0 {
                    Datum::Null
                } else {
                    #[allow(clippy::cast_precision_loss)]
                    Datum::Float8(*sum / (*count as f64))
                }
            }
            AggState::AvgNumeric { count, sum } => {
                if *count == 0 {
                    Datum::Null
                } else {
                    let c = numeric_of_i128(i128::from(*count))?;
                    Datum::Numeric(sum.checked_div(&c)?)
                }
            }
            AggState::MinMax { cur, .. } => cur.clone().unwrap_or(Datum::Null),
            AggState::BoolAnd(s) | AggState::BoolOr(s) => s.map_or(Datum::Null, Datum::Bool),
            AggState::StringAgg(s) => s.clone().map_or(Datum::Null, Datum::Text),
        })
    }

    /// 状態がヒープに持つ分の見積り（`size_of::<AggState>()` を除く）。値が大きくなった増分の課金に使う。
    pub fn heap_bytes(&self) -> usize {
        match self {
            AggState::SumNumeric(Some(n)) | AggState::AvgNumeric { sum: n, .. } => {
                estimate_numeric_bytes(n)
            }
            AggState::MinMax { cur: Some(d), .. } => {
                estimate_datum_bytes(d) - std::mem::size_of::<Datum>()
            }
            AggState::StringAgg(Some(s)) => s.len(),
            _ => 0,
        }
    }
}

fn cmp_sort_keys(spec: &[SortKey], a: &[Datum], b: &[Datum]) -> Ordering {
    for (k, (x, y)) in spec.iter().zip(a.iter().zip(b)) {
        let ord = match (x.is_null(), y.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => {
                if k.nulls_first {
                    Ordering::Less
                } else {
                    Ordering::Greater
                }
            }
            (false, true) => {
                if k.nulls_first {
                    Ordering::Greater
                } else {
                    Ordering::Less
                }
            }
            (false, false) => {
                let o = cmp_datum(x, y);
                if k.descending { o.reverse() } else { o }
            }
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

/// 集約 1 つの定義（`PhysAgg` の写し）。
#[derive(Debug, Clone)]
struct AggDef {
    kind: AggKind,
    args: Vec<PhysExpr>,
    filter: Option<PhysExpr>,
    distinct: bool,
    order_by: Vec<SortKey>,
}

/// `PhysAgg` の並びから作る、集約の評価の定義（種類・引数・FILTER・DISTINCT の写し）。
#[derive(Debug, Clone, Default)]
pub struct AggSet {
    aggs: Vec<AggDef>,
}

/// グループ 1 つの状態。
#[derive(Debug, Clone, Default)]
pub struct AggGroup {
    pub states: Vec<AggState>,
    /// DISTINCT の集合（DISTINCT でない集約は `None`）。
    distinct: Vec<Option<HashSet<HashKey>>>,
    /// ORDER BY つきの集約が溜める (キー, 引数)。`finish` で整列して遷移させる。
    buffered: Vec<Vec<(Vec<Datum>, Vec<Datum>)>>,
    /// このグループのために `ctx.mem` に課金した合計（DISTINCT の新しい値と状態の増分）。
    /// 捨てるときに呼び出し側が `release` する。
    charged: usize,
}

impl AggGroup {
    /// このグループのために課金した合計バイト数。
    pub fn charged(&self) -> usize {
        self.charged
    }
}

impl AggSet {
    pub fn new(aggs: &[PhysAgg]) -> AggSet {
        AggSet {
            aggs: aggs
                .iter()
                .map(|a| AggDef {
                    kind: a.kind,
                    args: a.args.clone(),
                    filter: a.filter.clone(),
                    distinct: a.distinct,
                    order_by: a.order_by.clone(),
                })
                .collect(),
        }
    }

    pub fn len(&self) -> usize {
        self.aggs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.aggs.is_empty()
    }

    pub fn new_group(&self) -> AggGroup {
        AggGroup {
            states: self.aggs.iter().map(|a| AggState::new(a.kind)).collect(),
            distinct: self
                .aggs
                .iter()
                .map(|a| a.distinct.then(HashSet::new))
                .collect(),
            buffered: vec![Vec::new(); self.aggs.len()],
            charged: 0,
        }
    }

    /// 1 グループの固定の見積りバイト数。
    pub fn base_bytes(&self) -> usize {
        std::mem::size_of::<AggGroup>()
            + self.aggs.len()
                * (std::mem::size_of::<AggState>()
                    + std::mem::size_of::<Option<HashSet<HashKey>>>())
    }

    /// 1 入力行を全集約に流す。DISTINCT の新しい値と、状態（`min` / `max` など）が大きくなった増分だけ
    /// `ctx.mem` に課金する（`g.charged()` に積む）。
    pub fn accumulate(&self, g: &mut AggGroup, row: &Row, ctx: &mut ExecCtx<'_>) -> Result<()> {
        for (i, a) in self.aggs.iter().enumerate() {
            if let Some(f) = &a.filter
                && eval::eval_pred(f, row, ctx)? != Some(true)
            {
                continue;
            }
            let mut args = Vec::with_capacity(a.args.len());
            let mut has_null = false;
            for e in &a.args {
                let v = eval::eval(e, row, ctx)?;
                has_null |= v.is_null();
                args.push(v);
            }
            // string_agg は区切りが NULL でも値があれば集める。
            let skip = if a.kind == AggKind::StringAgg {
                args.first().is_none_or(Datum::is_null)
            } else {
                has_null
            };
            if skip {
                continue;
            }
            if let Some(set) = g.distinct.get_mut(i).and_then(Option::as_mut) {
                let key = HashKey(args.clone());
                if set.contains(&key) {
                    continue;
                }
                let bytes = estimate_row_bytes(&args) + HASH_ENTRY_OVERHEAD;
                ctx.mem.charge(bytes)?;
                g.charged += bytes;
                set.insert(key);
            }
            if !a.order_by.is_empty() {
                let mut keys = Vec::with_capacity(a.order_by.len());
                for k in &a.order_by {
                    keys.push(eval::eval(&k.expr, row, ctx)?);
                }
                let bytes = estimate_row_bytes(&keys) + estimate_row_bytes(&args);
                ctx.mem.charge(bytes)?;
                g.charged += bytes;
                g.buffered[i].push((keys, args));
                continue;
            }
            let state = g
                .states
                .get_mut(i)
                .ok_or_else(|| Error::internal("aggregate group has too few states"))?;
            let before = state.heap_bytes();
            state.transition(&args)?;
            let after = state.heap_bytes();
            if after > before {
                ctx.mem.charge(after - before)?;
                g.charged += after - before;
            }
        }
        Ok(())
    }

    pub fn finish(&self, g: &AggGroup) -> Result<Vec<Datum>> {
        let mut out = Vec::with_capacity(g.states.len());
        for (i, st) in g.states.iter().enumerate() {
            let def = &self.aggs[i];
            if def.order_by.is_empty() {
                out.push(st.result()?);
                continue;
            }
            let mut rows: Vec<&(Vec<Datum>, Vec<Datum>)> = g.buffered[i].iter().collect();
            rows.sort_by(|a, b| cmp_sort_keys(&def.order_by, &a.0, &b.0));
            let mut state = st.clone();
            for (_, args) in rows {
                state.transition(args)?;
            }
            out.push(state.result()?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::executor::eval::tests::{col, int, lit, null};
    use crate::executor::nodes::test_util::Fixture;
    use crate::types::SqlType;

    fn num(s: &str) -> Datum {
        Datum::Numeric(Numeric::parse(s).unwrap())
    }

    fn run(kind: AggKind, inputs: &[Datum]) -> Datum {
        let mut s = AggState::new(kind);
        for d in inputs {
            s.transition(std::slice::from_ref(d)).unwrap();
        }
        s.result().unwrap()
    }

    fn show(d: &Datum) -> String {
        match d {
            Datum::Numeric(n) => n.to_string(),
            other => format!("{other:?}"),
        }
    }

    fn avg_ints(vals: &[i32]) -> String {
        let d: Vec<Datum> = vals.iter().map(|v| Datum::Int4(*v)).collect();
        show(&run(AggKind::AvgInt4, &d))
    }

    #[test]
    fn counts() {
        let mut s = AggState::new(AggKind::CountStar);
        assert_eq!(s.result().unwrap(), Datum::Int8(0));
        s.transition(&[]).unwrap();
        s.transition(&[]).unwrap();
        assert_eq!(s.result().unwrap(), Datum::Int8(2));
        // 非消費: 何度でも読める。
        assert_eq!(s.result().unwrap(), Datum::Int8(2));
        assert_eq!(run(AggKind::Count, &[]), Datum::Int8(0));
        assert_eq!(
            run(AggKind::Count, &[Datum::Text("a".into()), Datum::Int4(1)]),
            Datum::Int8(2)
        );
    }

    #[test]
    fn empty_input_gives_null_except_count() {
        for kind in [
            AggKind::SumInt2,
            AggKind::SumInt4,
            AggKind::SumInt8,
            AggKind::SumFloat4,
            AggKind::SumFloat8,
            AggKind::SumNumeric,
            AggKind::AvgInt2,
            AggKind::AvgInt4,
            AggKind::AvgInt8,
            AggKind::AvgFloat4,
            AggKind::AvgFloat8,
            AggKind::AvgNumeric,
            AggKind::Min,
            AggKind::Max,
            AggKind::BoolAnd,
            AggKind::BoolOr,
        ] {
            assert_eq!(
                AggState::new(kind).result().unwrap(),
                Datum::Null,
                "{kind:?}"
            );
        }
        assert_eq!(
            AggState::new(AggKind::Count).result().unwrap(),
            Datum::Int8(0)
        );
    }

    #[test]
    fn sum_result_types() {
        assert_eq!(
            run(AggKind::SumInt2, &[Datum::Int2(1), Datum::Int2(2)]),
            Datum::Int8(3)
        );
        assert_eq!(
            run(
                AggKind::SumInt4,
                &[Datum::Int4(i32::MAX), Datum::Int4(i32::MAX)]
            ),
            Datum::Int8(2 * i64::from(i32::MAX))
        );
        // sum(int8) は numeric: 9223372036854775807 + 1。
        assert_eq!(
            show(&run(
                AggKind::SumInt8,
                &[Datum::Int8(i64::MAX), Datum::Int8(1)]
            )),
            "9223372036854775808"
        );
        assert_eq!(
            run(
                AggKind::SumFloat4,
                &[Datum::Float4(1.5), Datum::Float4(2.0)]
            ),
            Datum::Float4(3.5)
        );
        assert_eq!(
            run(
                AggKind::SumFloat8,
                &[Datum::Float8(0.5), Datum::Float8(0.25)]
            ),
            Datum::Float8(0.75)
        );
        // numeric は入力の dscale の最大。
        assert_eq!(
            show(&run(AggKind::SumNumeric, &[num("1.10"), num("2.5")])),
            "3.60"
        );
    }

    #[test]
    fn sum_overflow_errors() {
        let mut s = AggState::new(AggKind::SumInt4);
        s.transition(&[Datum::Int4(1)]).unwrap();
        // i64 の最大に近づける（直接状態を作る）。
        let mut s2 = AggState::SumInt4(Some(i64::MAX));
        let e = s2.transition(&[Datum::Int4(1)]).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::NUMERIC_VALUE_OUT_OF_RANGE);
        assert_eq!(e.message, "bigint out of range");
        let mut s3 = AggState::SumInt2(Some(i64::MIN));
        assert!(s3.transition(&[Datum::Int2(-1)]).is_err());
        // sum(int8) は i128 なので桁あふれしない。
        let mut s4 = AggState::SumInt8(Some(i128::from(i64::MAX)));
        s4.transition(&[Datum::Int8(i64::MAX)]).unwrap();
    }

    #[test]
    fn float_overflow_only_when_inputs_are_finite() {
        let mut s = AggState::new(AggKind::SumFloat8);
        s.transition(&[Datum::Float8(f64::MAX)]).unwrap();
        let e = s.transition(&[Datum::Float8(f64::MAX)]).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::NUMERIC_VALUE_OUT_OF_RANGE);
        assert_eq!(e.message, "value out of range: overflow");
        let mut s = AggState::new(AggKind::SumFloat4);
        s.transition(&[Datum::Float4(f32::MAX)]).unwrap();
        assert!(s.transition(&[Datum::Float4(f32::MAX)]).is_err());
        let mut s = AggState::new(AggKind::AvgFloat8);
        s.transition(&[Datum::Float8(f64::MAX)]).unwrap();
        assert!(s.transition(&[Datum::Float8(f64::MAX)]).is_err());
        // 入力が無限大なら結果が無限大でもエラーにしない。
        let r = run(
            AggKind::SumFloat8,
            &[Datum::Float8(f64::INFINITY), Datum::Float8(1.0)],
        );
        assert_eq!(r, Datum::Float8(f64::INFINITY));
        let r = run(
            AggKind::SumFloat8,
            &[Datum::Float8(f64::NAN), Datum::Float8(1.0)],
        );
        assert!(r.as_f64().unwrap().is_nan());
    }

    #[test]
    fn avg_float8_overflows_on_sxx_like_postgres() {
        let mut s = AggState::new(AggKind::AvgFloat8);
        for v in [1.0, 0.0] {
            s.transition(&[Datum::Float8(v)]).unwrap();
        }
        let e = s.transition(&[Datum::Float8(1e300)]).unwrap_err();
        assert_eq!(e.message, "value out of range: overflow");
    }

    #[test]
    fn avg_float_is_float8() {
        assert_eq!(
            run(
                AggKind::AvgFloat4,
                &[Datum::Float4(1.0), Datum::Float4(2.0)]
            ),
            Datum::Float8(1.5)
        );
        assert_eq!(
            run(
                AggKind::AvgFloat8,
                &[Datum::Float8(1.0), Datum::Float8(2.0)]
            ),
            Datum::Float8(1.5)
        );
    }

    /// 05 §6.4 の実機の照合値。
    #[test]
    fn avg_numeric_scale_matches_postgres() {
        assert_eq!(avg_ints(&[1, 2]), "1.5000000000000000");
        assert_eq!(avg_ints(&[1, 2, 4]), "2.3333333333333333");
        assert_eq!(avg_ints(&[1, 1, 1]), "1.00000000000000000000");
        assert_eq!(avg_ints(&[0, 1, 1]), "0.66666666666666666667");
        assert_eq!(avg_ints(&[1, 1, 2, 1_000_000]), "250001.000000000000");
        assert_eq!(avg_ints(&[2]), "2.0000000000000000");
        let d = [Datum::Int8(1), Datum::Int8(2)];
        assert_eq!(show(&run(AggKind::AvgInt8, &d)), "1.5000000000000000");
        let d = [Datum::Int2(1), Datum::Int2(2)];
        assert_eq!(show(&run(AggKind::AvgInt2, &d)), "1.5000000000000000");
    }

    #[test]
    fn avg_numeric_kind() {
        assert_eq!(
            show(&run(AggKind::AvgNumeric, &[num("1.5"), num("2.5")])),
            "2.0000000000000000"
        );
        assert_eq!(
            show(&run(AggKind::AvgNumeric, &[num("1"), num("2")])),
            "1.5000000000000000"
        );
    }

    /// C-4: 等しいときは後に来た値が代表。
    #[test]
    fn min_max_keep_the_later_value_on_ties() {
        let dscale = |d: &Datum| match d {
            Datum::Numeric(n) => n.scale().unwrap(),
            _ => panic!(),
        };
        let vals = [num("1.10"), num("1.1"), num("1.100")];
        assert_eq!(dscale(&run(AggKind::Max, &vals)), 3);
        assert_eq!(dscale(&run(AggKind::Min, &vals)), 3);
        let r = run(AggKind::Max, &[num("1.5"), num("1.10")]);
        assert_eq!(show(&r), "1.5");
        // bpchar は末尾の空白を無視して比べ、元の値を返す。
        let r = run(
            AggKind::Min,
            &[Datum::BpChar("a  ".into()), Datum::BpChar("a".into())],
        );
        assert_eq!(r, Datum::BpChar("a".into()));
        // text は C 照合のバイト順、NaN は最大。
        assert_eq!(
            run(
                AggKind::Max,
                &[
                    Datum::Text("b".into()),
                    Datum::Text("B".into()),
                    Datum::Text("a".into())
                ]
            ),
            Datum::Text("b".into())
        );
        let r = run(
            AggKind::Max,
            &[
                Datum::Float8(1.0),
                Datum::Float8(f64::NAN),
                Datum::Float8(2.0),
            ],
        );
        assert!(r.as_f64().unwrap().is_nan());
        let r = run(AggKind::Min, &[Datum::Float8(f64::NAN), Datum::Float8(2.0)]);
        assert_eq!(r, Datum::Float8(2.0));
        // -0 と +0 は等しいので後の値。
        let r = run(AggKind::Max, &[Datum::Float8(0.0), Datum::Float8(-0.0)]);
        assert!(r.as_f64().unwrap().is_sign_negative());
    }

    #[test]
    fn bool_aggregates() {
        let t = Datum::Bool(true);
        let f = Datum::Bool(false);
        assert_eq!(run(AggKind::BoolAnd, &[t.clone(), t.clone()]), t);
        assert_eq!(run(AggKind::BoolAnd, &[t.clone(), f.clone()]), f);
        assert_eq!(run(AggKind::BoolOr, &[f.clone(), f.clone()]), f);
        assert_eq!(run(AggKind::BoolOr, &[f.clone(), t.clone()]), t);
    }

    #[test]
    fn bad_arguments_are_internal_errors() {
        let mut s = AggState::new(AggKind::SumInt4);
        assert_eq!(
            s.transition(&[Datum::Text("x".into())])
                .unwrap_err()
                .sqlstate,
            sqlstate::INTERNAL_ERROR
        );
        assert!(s.transition(&[]).is_err());
    }

    fn agg(
        kind: AggKind,
        args: Vec<PhysExpr>,
        distinct: bool,
        filter: Option<PhysExpr>,
    ) -> PhysAgg {
        PhysAgg {
            kind,
            arg_types: vec![],
            args,
            distinct,
            filter,
            order_by: vec![],
            result: SqlType::INT8,
        }
    }

    #[test]
    fn accumulate_filter_null_distinct() {
        use crate::executor::eval::tests::{GT, op};
        let set = AggSet::new(&[
            agg(AggKind::CountStar, vec![], false, None),
            agg(AggKind::Count, vec![col(0, SqlType::INT4)], false, None),
            agg(AggKind::Count, vec![col(0, SqlType::INT4)], true, None),
            agg(AggKind::SumInt4, vec![col(0, SqlType::INT4)], true, None),
            // FILTER (WHERE x > 1)
            agg(
                AggKind::CountStar,
                vec![],
                false,
                Some(op(&GT, col(0, SqlType::INT4), int(1))),
            ),
        ]);
        let mut f = Fixture::new();
        let mut ctx = f.ctx();
        let mut g = set.new_group();
        for v in [Datum::Int4(1), Datum::Int4(2), Datum::Null, Datum::Int4(2)] {
            set.accumulate(&mut g, &vec![v], &mut ctx).unwrap();
        }
        let out = set.finish(&g).unwrap();
        assert_eq!(
            out,
            vec![
                Datum::Int8(4),
                Datum::Int8(3),
                Datum::Int8(2),
                Datum::Int8(3),
                // FILTER: NULL > 1 は NULL で落ちる。2 と 2 の 2 行。
                Datum::Int8(2),
            ]
        );
        // DISTINCT の新しい値 (1, 2) × 2 集約だけが課金される。
        assert!(g.charged() > 0);
        assert_eq!(ctx.mem.used(), g.charged());
        ctx.mem.release(g.charged());
        assert_eq!(ctx.mem.used(), 0);
    }

    #[test]
    fn filter_dropping_everything_gives_empty_results() {
        use crate::executor::eval::tests::{GT, op};
        let set = AggSet::new(&[
            agg(
                AggKind::CountStar,
                vec![],
                false,
                Some(op(&GT, col(0, SqlType::INT4), int(100))),
            ),
            agg(
                AggKind::SumInt4,
                vec![col(0, SqlType::INT4)],
                false,
                Some(op(&GT, col(0, SqlType::INT4), int(100))),
            ),
        ]);
        let mut f = Fixture::new();
        let mut ctx = f.ctx();
        let mut g = set.new_group();
        set.accumulate(&mut g, &vec![Datum::Int4(1)], &mut ctx)
            .unwrap();
        assert_eq!(set.finish(&g).unwrap(), vec![Datum::Int8(0), Datum::Null]);
    }

    #[test]
    fn multi_arg_null_skips_and_literal_args() {
        // 引数のどれかが NULL なら飛ばす（count(a) の a が NULL）。
        let set = AggSet::new(&[agg(
            AggKind::Count,
            vec![lit(Datum::Int4(7), SqlType::INT4)],
            false,
            None,
        )]);
        let mut f = Fixture::new();
        let mut ctx = f.ctx();
        let mut g = set.new_group();
        set.accumulate(&mut g, &vec![], &mut ctx).unwrap();
        assert_eq!(set.finish(&g).unwrap(), vec![Datum::Int8(1)]);
        let set = AggSet::new(&[agg(AggKind::Count, vec![null(SqlType::INT4)], false, None)]);
        let mut g = set.new_group();
        set.accumulate(&mut g, &vec![], &mut ctx).unwrap();
        assert_eq!(set.finish(&g).unwrap(), vec![Datum::Int8(0)]);
    }

    #[test]
    fn distinct_charge_failure_is_53200_and_value_not_added() {
        let set = AggSet::new(&[agg(AggKind::Count, vec![col(0, SqlType::INT4)], true, None)]);
        let mut f = Fixture::new();
        f.mem_limit = 10;
        let mut ctx = f.ctx();
        let mut g = set.new_group();
        let e = set
            .accumulate(&mut g, &vec![Datum::Int4(1)], &mut ctx)
            .unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::OUT_OF_MEMORY);
        assert_eq!(g.charged(), 0);
        assert_eq!(ctx.mem.used(), 0);
    }

    #[test]
    fn min_max_growth_is_charged() {
        let set = AggSet::new(&[agg(AggKind::Max, vec![col(0, SqlType::TEXT)], false, None)]);
        let mut f = Fixture::new();
        let mut ctx = f.ctx();
        let mut g = set.new_group();
        set.accumulate(&mut g, &vec![Datum::Text("a".into())], &mut ctx)
            .unwrap();
        let c1 = g.charged();
        assert_eq!(c1, 1);
        set.accumulate(&mut g, &vec![Datum::Text("bbbb".into())], &mut ctx)
            .unwrap();
        assert_eq!(g.charged(), 4);
        // 小さくなっても返さない（増分だけ課金）。
        set.accumulate(&mut g, &vec![Datum::Text("bbbb".into())], &mut ctx)
            .unwrap();
        assert_eq!(g.charged(), 4);
        assert_eq!(ctx.mem.used(), 4);
        assert!(set.base_bytes() > 0);
    }
}

/// ノードのテストが使う `PhysAgg` の組み立て。
#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use crate::types::SqlType;

    pub(crate) fn agg(
        kind: AggKind,
        args: Vec<PhysExpr>,
        distinct: bool,
        filter: Option<PhysExpr>,
    ) -> PhysAgg {
        PhysAgg {
            kind,
            arg_types: vec![],
            args,
            distinct,
            filter,
            order_by: vec![],
            result: SqlType::INT8,
        }
    }
}
