//! インデックス選択（`m4/04` §3.7・§7.3・§7.4。持ち主は L2）。
//!
//! 述語（`Filter` の conjunct）から B+Tree インデックスで使える条件（[`IndexQual`]）を取り出し、
//! インデックスごとの使い方（[`IndexPick`]）にしてスコアの最大を選ぶ。`enable_*`（§9）の適用とソートの省略
//! （`order_satisfied`）もここ。値の式は呼び出し側（`physicalize`）が `layout = []` の文脈で降ろす。
//!
//! 04 の署名からの差（実装の都合）: `extract_quals` / `choose_scan` / `is_point_lookup` は `CatalogReader` を取らない
//! （演算子の戦略は `catalog::opclass` の静的な表で引ける）。`ScanRequest` に `system_cols`（値の式が参照してはいけない
//! 走査自身の列）と `force_order`（§7.4 の (b)）を足した。

use super::PlannerSettings;
use super::logical::LExpr;
use super::physical::ScanDirection;
use super::util::{ColSet, Volatility, expr_refs, expr_volatility};
use crate::catalog::{CastMethod, IndexDef, TableDef, builtin, opclass};
use crate::expr::{ColId, ExprKind};
use crate::storage::RelHandle;
use crate::types::Oid;

/// 述語から取り出した、インデックスで使える条件。
#[derive(Clone, Debug)]
pub struct IndexQual {
    pub attnum: i16,
    pub kind: QualKind,
    /// 値の式（定数式: この走査の列を含まない。パラメータ・InitPlan・外側の列を含んでよい）。`IsNull` では `None`。
    pub value: Option<LExpr>,
    /// 値の式が演算子の何番目の引数か（`conjuncts[conj]` の `Operator.args[value_arg]`）。`IsNull` では 0。
    pub value_arg: usize,
    /// 述語の添字（`conjuncts` の）。
    pub conj: usize,
}

/// 戦略 3 / `IS NULL` / 1 / 2 / 4 / 5。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum QualKind {
    Eq,
    IsNull,
    Lt,
    Le,
    Ge,
    Gt,
}

impl QualKind {
    fn from_strategy(s: u8) -> Option<QualKind> {
        Some(match s {
            1 => QualKind::Lt,
            2 => QualKind::Le,
            3 => QualKind::Eq,
            4 => QualKind::Ge,
            5 => QualKind::Gt,
            _ => return None,
        })
    }

    /// `Index Cond` に書く演算子（`IsNull` は専用の書式）。
    pub fn symbol(self) -> &'static str {
        match self {
            QualKind::Eq => "=",
            QualKind::IsNull => "IS NULL",
            QualKind::Lt => "<",
            QualKind::Le => "<=",
            QualKind::Ge => ">=",
            QualKind::Gt => ">",
        }
    }
}

/// 選んだインデックスと、その使い方。
#[derive(Clone, Debug)]
pub struct IndexPick {
    /// `RelHandle.indexes` の添字（`TableDef.indexes` と同じ OID 昇順）。
    pub index: usize,
    /// 先頭の列から連続（`Eq` か `IsNull`）。
    pub eq: Vec<IndexQual>,
    /// `eq.len()` 番目の列の下限（`Gt` / `Ge`）。
    pub lower: Option<IndexQual>,
    /// 同上限（`Lt` / `Le`）。
    pub upper: Option<IndexQual>,
    pub unique_full: bool,
    pub direction: ScanDirection,
    /// 要求された順序（`want_order`）を走査順で満たす。
    pub ordered: bool,
}

impl IndexPick {
    /// `(unique_full, eq の数, 境界の数)`。辞書順で大きいほうがよい。
    pub fn score(&self) -> (u8, usize, usize) {
        (
            u8::from(self.unique_full),
            self.eq.len(),
            usize::from(self.lower.is_some()) + usize::from(self.upper.is_some()),
        )
    }

    /// 使った述語（`conjuncts` の添字）。キー列の順に `eq`、続けて `lower`、`upper`。
    pub fn used(&self) -> Vec<usize> {
        self.eq
            .iter()
            .chain(self.lower.iter())
            .chain(self.upper.iter())
            .map(|q| q.conj)
            .collect()
    }

    /// 値の式を持つ述語を 1 つでも使っているか（`pred` が真の述語）。
    pub fn uses(&self, mut pred: impl FnMut(&IndexQual) -> bool) -> bool {
        self.eq
            .iter()
            .chain(self.lower.iter())
            .chain(self.upper.iter())
            .any(&mut pred)
    }
}

/// 順序の要求の 1 項目。
#[derive(Clone, Copy, Debug)]
pub struct OrderKey {
    pub col: ColId,
    pub descending: bool,
    pub nulls_first: bool,
}

#[derive(Debug)]
pub struct ScanRequest<'a> {
    pub table: &'a TableDef,
    pub rel: &'a RelHandle,
    /// `Get` のユーザー列（attnum 順）の `ColId`。
    pub cols: &'a [ColId],
    /// `Get` のシステム列の `ColId`（値の式が参照してはいけない）。
    pub system_cols: &'a [ColId],
    /// 述語（`Filter` の conjunct。元の順）。
    pub conjuncts: &'a [LExpr],
    /// 内側 Index Scan のとき、外側の列。ここに含まれる列は「値の式」の中で使える（この集合に
    /// 入っていない列はパラメータ・InitPlan など、走査より外側のものとして同様に使える）。
    pub outer_cols: &'a ColSet,
    /// ソートの省略を試すときの要求順序（§7.4）。
    pub want_order: Option<&'a [OrderKey]>,
    /// 述語で選んだインデックスが順序を満たさなくても、満たすインデックスを選び直す（§7.4 の (b)）。
    pub force_order: bool,
}

fn strip_binary_cast(e: &LExpr) -> &LExpr {
    match &e.kind {
        ExprKind::Cast {
            expr,
            method: CastMethod::Binary,
            ..
        } => strip_binary_cast(expr),
        _ => e,
    }
}

/// 列式（ユーザー列、または `Binary` キャストで包んだもの）の attnum。
fn column_attnum(e: &LExpr, table: &TableDef, cols: &[ColId]) -> Option<i16> {
    match &strip_binary_cast(e).kind {
        ExprKind::Column(c) => {
            let i = cols.iter().position(|x| x == c)?;
            table.columns.get(i).map(|d| d.attnum)
        }
        _ => None,
    }
}

/// 値の式として使えるか: この走査自身の列を含まず、揮発性でなく、集約を含まない。
fn is_value_expr(e: &LExpr, own: &ColSet) -> bool {
    !e.contains_aggregate()
        && !super::util::contains_sublink(e)
        && expr_volatility(e) != Volatility::Volatile
        && expr_refs(e).is_disjoint(own)
}

/// 述語 `c` がインデックス列 `attnum`（族 `family`）の条件になるか。
fn qual_for(
    c: &LExpr,
    conj: usize,
    attnum: i16,
    family: Oid,
    table: &TableDef,
    cols: &[ColId],
    own: &ColSet,
) -> Option<IndexQual> {
    match &c.kind {
        ExprKind::Operator { op, args } if args.len() == 2 => {
            let (x, y) = (&args[0], &args[1]);
            if column_attnum(x, table, cols) == Some(attnum)
                && is_value_expr(y, own)
                && let Some((s, lt, rt)) = opclass::operator_strategy(op.oid, family)
                && x.ty.oid == lt
                && y.ty.oid == rt
            {
                return Some(IndexQual {
                    attnum,
                    kind: QualKind::from_strategy(s)?,
                    value: Some(y.clone()),
                    value_arg: 1,
                    conj,
                });
            }
            if column_attnum(y, table, cols) == Some(attnum) && is_value_expr(x, own) {
                let com = builtin::operator_meta(op.oid)?.com;
                if com == 0 {
                    return None;
                }
                let (s, lt, rt) = opclass::operator_strategy(com, family)?;
                if y.ty.oid == lt && x.ty.oid == rt {
                    return Some(IndexQual {
                        attnum,
                        kind: QualKind::from_strategy(s)?,
                        value: Some(x.clone()),
                        value_arg: 0,
                        conj,
                    });
                }
            }
            None
        }
        ExprKind::IsNull(x) if column_attnum(x, table, cols) == Some(attnum) => Some(IndexQual {
            attnum,
            kind: QualKind::IsNull,
            value: None,
            value_arg: 0,
            conj,
        }),
        _ => None,
    }
}

fn own_cols(req: &ScanRequest<'_>) -> ColSet {
    req.cols
        .iter()
        .chain(req.system_cols.iter())
        .copied()
        .collect()
}

/// インデックスごと（`table.indexes` の添字）の候補（キー列の順、同じ列は述語の順）。
pub fn extract_quals(req: &ScanRequest<'_>) -> Vec<(usize, Vec<IndexQual>)> {
    let own = own_cols(req);
    req.table
        .indexes
        .iter()
        .enumerate()
        .map(|(i, def)| {
            let mut quals = Vec::new();
            for col in &def.columns {
                for (ci, c) in req.conjuncts.iter().enumerate() {
                    if let Some(q) =
                        qual_for(c, ci, col.attnum, col.opfamily, req.table, req.cols, &own)
                    {
                        quals.push(q);
                    }
                }
            }
            (i, quals)
        })
        .collect()
}

/// 候補からインデックスの使い方を作る。使える条件がなければ `None`（`allow_empty` なら空の使い方）。
fn pick_from(
    index: usize,
    def: &IndexDef,
    quals: &[IndexQual],
    allow_empty: bool,
) -> Option<IndexPick> {
    let mut eq: Vec<IndexQual> = Vec::new();
    for col in &def.columns {
        match quals
            .iter()
            .find(|q| q.attnum == col.attnum && matches!(q.kind, QualKind::Eq | QualKind::IsNull))
        {
            Some(q) => eq.push(q.clone()),
            None => break,
        }
    }
    let (mut lower, mut upper) = (None, None);
    if let Some(col) = def.columns.get(eq.len()) {
        lower = quals
            .iter()
            .find(|q| q.attnum == col.attnum && matches!(q.kind, QualKind::Gt | QualKind::Ge))
            .cloned();
        upper = quals
            .iter()
            .find(|q| q.attnum == col.attnum && matches!(q.kind, QualKind::Lt | QualKind::Le))
            .cloned();
    }
    if eq.is_empty() && lower.is_none() && upper.is_none() && !allow_empty {
        return None;
    }
    let unique_full = def.unique
        && eq.len() == def.columns.len()
        && !eq.iter().any(|q| q.kind == QualKind::IsNull);
    Some(IndexPick {
        index,
        eq,
        lower,
        upper,
        unique_full,
        direction: ScanDirection::Forward,
        ordered: false,
    })
}

/// 順序要求 `keys` がインデックスの走査順で満たされるか。満たせば走査の向き（§7.4）。
/// `eq_cols` は等値で固定された列の attnum、`cols` は `(ColId, attnum)`。
pub fn order_satisfied(
    index: &IndexDef,
    eq_cols: &[i16],
    keys: &[OrderKey],
    cols: &[(ColId, i16)],
) -> Option<ScanDirection> {
    let mut j = eq_cols.len();
    let mut dir: Option<ScanDirection> = None;
    for k in keys {
        let attnum = cols.iter().find(|(c, _)| *c == k.col)?.1;
        if eq_cols.contains(&attnum) {
            continue;
        }
        let ic = index.columns.get(j)?;
        j += 1;
        if ic.attnum != attnum {
            return None;
        }
        let d = if (k.descending, k.nulls_first) == (ic.descending, ic.nulls_first) {
            ScanDirection::Forward
        } else if (!k.descending, !k.nulls_first) == (ic.descending, ic.nulls_first) {
            ScanDirection::Backward
        } else {
            return None;
        };
        match dir {
            None => dir = Some(d),
            Some(x) if x != d => return None,
            Some(_) => {}
        }
    }
    Some(dir.unwrap_or(ScanDirection::Forward))
}

fn col_attnums(req: &ScanRequest<'_>) -> Vec<(ColId, i16)> {
    req.cols
        .iter()
        .zip(&req.table.columns)
        .map(|(c, d)| (*c, d.attnum))
        .collect()
}

fn mark_order(pick: &mut IndexPick, req: &ScanRequest<'_>, keys: &[OrderKey]) {
    let def = &req.table.indexes[pick.index];
    let eq_cols: Vec<i16> = pick.eq.iter().map(|q| q.attnum).collect();
    if let Some(d) = order_satisfied(def, &eq_cols, keys, &col_attnums(req)) {
        pick.direction = d;
        pick.ordered = true;
    }
}

/// `enable_*` を適用して、使うなら `Some`。`None` は Seq Scan（§7.3 の 5、§7.4）。
pub fn choose_scan(req: &ScanRequest<'_>, settings: &PlannerSettings) -> Option<IndexPick> {
    let use_index = settings.enable_indexscan || !settings.enable_seqscan;
    if !use_index || req.table.indexes.is_empty() {
        return None;
    }
    let mut best: Option<IndexPick> = None;
    for (i, quals) in extract_quals(req) {
        if let Some(p) = pick_from(i, &req.table.indexes[i], &quals, false)
            && best.as_ref().is_none_or(|b| p.score() > b.score())
        {
            best = Some(p);
        }
    }
    if let Some(keys) = req.want_order {
        if let Some(b) = best.as_mut() {
            mark_order(b, req, keys);
        }
        if req.force_order && !best.as_ref().is_some_and(|b| b.ordered) {
            let mut forced: Option<IndexPick> = None;
            for (i, quals) in extract_quals(req) {
                if let Some(mut p) = pick_from(i, &req.table.indexes[i], &quals, true) {
                    mark_order(&mut p, req, keys);
                    if p.ordered && forced.as_ref().is_none_or(|b| p.score() > b.score()) {
                        forced = Some(p);
                    }
                }
            }
            if forced.is_some() {
                best = forced;
            }
        }
    }
    best
}

/// 一意インデックスの全列が定数の等値か（`size::estimate` が使う）。`IS NULL` は数えない。
pub fn is_point_lookup(table: &TableDef, cols: &[ColId], conjuncts: &[LExpr]) -> bool {
    let own: ColSet = cols.iter().copied().collect();
    table.indexes.iter().any(|def| {
        def.unique
            && !def.columns.is_empty()
            && def.columns.iter().all(|col| {
                conjuncts.iter().enumerate().any(|(ci, c)| {
                    qual_for(c, ci, col.attnum, col.opfamily, table, cols, &own)
                        .is_some_and(|q| q.kind == QualKind::Eq)
                })
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::fake::{FakeCatalog, TableBuilder};
    use crate::catalog::{BuiltinOperator, builtin};
    use crate::error::Span;
    use crate::types::{Datum, SqlType};
    use std::sync::Arc;

    fn op(name: &str, l: SqlType, r: SqlType) -> &'static BuiltinOperator {
        builtin::operators_named(name)
            .into_iter()
            .find(|o| o.left == Some(l.oid) && o.right == r.oid)
            .unwrap_or_else(|| panic!("operator {name}"))
    }

    fn cmp(name: &str, a: LExpr, b: LExpr) -> LExpr {
        let o = op(name, a.ty, b.ty);
        LExpr::new(
            ExprKind::Operator {
                op: o,
                args: vec![a, b],
            },
            SqlType::BOOL,
            Span::default(),
        )
    }

    fn col(i: u32) -> LExpr {
        LExpr::column(ColId(i), SqlType::INT4)
    }

    fn int(v: i32) -> LExpr {
        LExpr::literal(Datum::Int4(v), SqlType::INT4)
    }

    /// `t(a, b, c)` with `t_pkey(a)`, `t_b_c(b, c)` and `t_c_desc(c DESC)`.
    fn table() -> (Arc<TableDef>, RelHandle) {
        let mut cat = FakeCatalog::new("postgres");
        let t = cat.add(
            &TableBuilder::new("t")
                .column_nn("a", SqlType::INT4)
                .column("b", SqlType::INT4)
                .column("c", SqlType::INT4)
                .primary_key(&["a"])
                .index(Some("t_b_c"), &["b", "c"], false)
                .index_ordered(Some("t_c_desc"), &[("c", true, true)], false),
        );
        let rel = RelHandle::from_table(&t);
        (t, rel)
    }

    fn cols() -> Vec<ColId> {
        vec![ColId(0), ColId(1), ColId(2)]
    }

    fn pick(
        conjuncts: &[LExpr],
        settings: &PlannerSettings,
        want_order: Option<&[OrderKey]>,
        force: bool,
    ) -> Option<IndexPick> {
        let (t, rel) = table();
        let cols = cols();
        let outer = ColSet::new();
        let req = ScanRequest {
            table: &t,
            rel: &rel,
            cols: &cols,
            system_cols: &[],
            conjuncts,
            outer_cols: &outer,
            want_order,
            force_order: force,
        };
        choose_scan(&req, settings)
    }

    fn name_of(p: &IndexPick) -> String {
        table().0.indexes[p.index].name.clone()
    }

    #[test]
    fn unique_full_equality_beats_everything() {
        let s = PlannerSettings::default();
        let p = pick(
            &[cmp("=", col(0), int(5)), cmp("=", col(1), int(1))],
            &s,
            None,
            false,
        )
        .unwrap();
        assert_eq!(name_of(&p), "t_pkey");
        assert!(p.unique_full);
        assert_eq!(p.used(), vec![0]);
        assert_eq!(p.score(), (1, 1, 0));
    }

    #[test]
    fn more_equality_columns_win_then_oid_order() {
        let s = PlannerSettings::default();
        let conj = [cmp("=", col(1), int(1)), cmp("=", col(2), int(2))];
        let p = pick(&conj, &s, None, false).unwrap();
        assert_eq!(name_of(&p), "t_b_c");
        assert_eq!(p.eq.len(), 2);
        // 同点（どちらも先頭列の等値 1 つ）は OID が小さいほう（t_pkey を使えない条件で確かめる）。
        let conj = [cmp("=", col(2), int(2))];
        let p = pick(&conj, &s, None, false).unwrap();
        assert_eq!(name_of(&p), "t_c_desc");
    }

    #[test]
    fn range_and_commuted_comparison() {
        let s = PlannerSettings::default();
        // 5 < a AND a <= 9
        let conj = [cmp("<", int(5), col(0)), cmp("<=", col(0), int(9))];
        let p = pick(&conj, &s, None, false).unwrap();
        assert_eq!(name_of(&p), "t_pkey");
        assert!(p.eq.is_empty());
        assert_eq!(p.lower.as_ref().unwrap().kind, QualKind::Gt);
        assert_eq!(p.lower.as_ref().unwrap().value_arg, 0);
        assert_eq!(p.upper.as_ref().unwrap().kind, QualKind::Le);
        assert_eq!(p.score(), (0, 0, 2));
    }

    #[test]
    fn is_null_and_unusable_predicates() {
        let s = PlannerSettings::default();
        let isnull = LExpr::new(
            ExprKind::IsNull(Box::new(col(1))),
            SqlType::BOOL,
            Span::default(),
        );
        let p = pick(&[isnull], &s, None, false).unwrap();
        assert_eq!(p.eq[0].kind, QualKind::IsNull);
        assert!(!p.unique_full);
        // <>、列どうし、式、OR は候補にならない。
        let ne = cmp("<>", col(0), int(1));
        let colcol = cmp("=", col(0), col(1));
        let sum = {
            let plus = op("+", SqlType::INT4, SqlType::INT4);
            LExpr::new(
                ExprKind::Operator {
                    op: plus,
                    args: vec![col(0), int(1)],
                },
                SqlType::INT4,
                Span::default(),
            )
        };
        let expr_eq = cmp("=", sum, int(3));
        let or = LExpr::new(
            ExprKind::Or(vec![cmp("=", col(0), int(1)), cmp("=", col(0), int(2))]),
            SqlType::BOOL,
            Span::default(),
        );
        assert!(pick(&[ne, colcol, expr_eq, or], &s, None, false).is_none());
    }

    #[test]
    fn value_may_reference_outer_columns_but_not_own() {
        let (t, rel) = table();
        let cols = cols();
        let mut outer = ColSet::new();
        outer.insert(ColId(10));
        let conj = [cmp("=", col(0), col(10))];
        let req = ScanRequest {
            table: &t,
            rel: &rel,
            cols: &cols,
            system_cols: &[],
            conjuncts: &conj,
            outer_cols: &outer,
            want_order: None,
            force_order: false,
        };
        let p = choose_scan(&req, &PlannerSettings::default()).unwrap();
        assert!(p.eq[0].value.is_some());
        // 自分の列との等値は使えない。
        let conj = [cmp("=", col(0), col(1))];
        let req = ScanRequest {
            conjuncts: &conj,
            ..req
        };
        assert!(choose_scan(&req, &PlannerSettings::default()).is_none());
    }

    #[allow(clippy::field_reassign_with_default)]
    #[test]
    fn enable_flags() {
        let conj = [cmp("=", col(0), int(5))];
        let mut s = PlannerSettings::default();
        s.enable_indexscan = false;
        assert!(pick(&conj, &s, None, false).is_none());
        s.enable_seqscan = false;
        assert!(pick(&conj, &s, None, false).is_some());
        s.enable_indexscan = true;
        assert!(pick(&conj, &s, None, false).is_some());
        // 候補がなければ設定に関わらず Seq Scan。
        assert!(pick(&[cmp("<>", col(0), int(1))], &s, None, false).is_none());
    }

    #[test]
    fn order_satisfied_directions() {
        let (t, _) = table();
        let cols: Vec<(ColId, i16)> = vec![(ColId(0), 1), (ColId(1), 2), (ColId(2), 3)];
        let key = |c: u32, d: bool, n: bool| OrderKey {
            col: ColId(c),
            descending: d,
            nulls_first: n,
        };
        let pkey = &t.indexes[0];
        assert_eq!(
            order_satisfied(pkey, &[], &[key(0, false, false)], &cols),
            Some(ScanDirection::Forward)
        );
        assert_eq!(
            order_satisfied(pkey, &[], &[key(0, true, true)], &cols),
            Some(ScanDirection::Backward)
        );
        // NULLS の向きだけ違うものは満たせない。
        assert_eq!(
            order_satisfied(pkey, &[], &[key(0, false, true)], &cols),
            None
        );
        // 等値で固定された列は飛ばす（b = 1 ORDER BY b, c）。
        let bc = &t.indexes[1];
        assert_eq!(
            order_satisfied(
                bc,
                &[2],
                &[key(1, false, false), key(2, false, false)],
                &cols
            ),
            Some(ScanDirection::Forward)
        );
        // 向きが混ざるものは不可。
        assert_eq!(
            order_satisfied(bc, &[], &[key(1, false, false), key(2, true, true)], &cols),
            None
        );
        // DESC NULLS FIRST の索引は DESC NULLS FIRST で前向き、ASC NULLS LAST で後ろ向き。
        let cd = &t.indexes[2];
        assert_eq!(
            order_satisfied(cd, &[], &[key(2, true, true)], &cols),
            Some(ScanDirection::Forward)
        );
        assert_eq!(
            order_satisfied(cd, &[], &[key(2, false, false)], &cols),
            Some(ScanDirection::Backward)
        );
    }

    #[test]
    fn ordered_scan_with_and_without_predicates() {
        let s = PlannerSettings::default();
        let want = [OrderKey {
            col: ColId(0),
            descending: false,
            nulls_first: false,
        }];
        // (a) 述語で選んだ pkey が順序も満たす。
        let conj = [cmp(">", col(0), int(5))];
        let p = pick(&conj, &s, Some(&want), false).unwrap();
        assert!(p.ordered);
        assert_eq!(p.direction, ScanDirection::Forward);
        // 述語がなければ force_order のときだけ全インデックス走査を選ぶ。
        assert!(pick(&[], &s, Some(&want), false).is_none());
        let p = pick(&[], &s, Some(&want), true).unwrap();
        assert!(p.ordered && p.eq.is_empty() && p.lower.is_none() && p.upper.is_none());
        assert_eq!(name_of(&p), "t_pkey");
        // 述語で選んだインデックスが順序を満たさなければ、force のとき満たすものに替える。
        let conj = [cmp("=", col(1), int(1))];
        let p = pick(&conj, &s, Some(&want), false).unwrap();
        assert!(!p.ordered);
        let p = pick(&conj, &s, Some(&want), true).unwrap();
        assert!(p.ordered);
        assert_eq!(name_of(&p), "t_pkey");
    }

    #[test]
    fn point_lookup() {
        let (t, _) = table();
        let cols = cols();
        assert!(is_point_lookup(&t, &cols, &[cmp("=", col(0), int(1))]));
        assert!(!is_point_lookup(&t, &cols, &[cmp("<", col(0), int(1))]));
        // 一意でないインデックスの全列等値は数えない。
        assert!(!is_point_lookup(
            &t,
            &cols,
            &[cmp("=", col(1), int(1)), cmp("=", col(2), int(1))]
        ));
    }
}
