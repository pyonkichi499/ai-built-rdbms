//! `FOR UPDATE / NO KEY UPDATE / SHARE / KEY SHARE` の解析（M5 RW-3a。`spec/design/m5/02-row-lock-rr.md` §6.4）。
//!
//! 最上位の SELECT だけが `FOR` を持てる（RW-D9）。[`Analyzer::analyze_top_query`] が最上位の `FOR` 句を
//! 外して本体を `analyze_query` で解析し、できた [`BoundSelect`] に対して句を解決して `BoundSelect.locking`
//! へ入れる。外さずに `analyze_query` へ届いた `FOR` は入れ子の中のもので、`0A000`（[`nested_locking_error`]）。
//!
//! 解析前の判定は [`locked_table_refs`]（LK がリレーションロックの強度を決めるのに使う）。

use super::Analyzer;
use super::bound::{
    BoundLockingClause, BoundQuery, BoundSelect, BoundSetExpr, FromItem, JoinType, RteKind,
};
use super::scope::QueryEnv;
use crate::catalog::RelKind;
use crate::error::{Error, Result, sqlstate};
use crate::expr::RteId;
use crate::sql::ast::{
    LockStrength, LockWaitPolicy, LockingClause, ObjectName, Query, QueryBody, TableRef,
};
use crate::storage::RowWait;
use crate::txn::LockTupleMode;

/// 入れ子の `FOR`（FROM の副問い合わせ・CTE・SubLink・集合演算の腕の内側。§6.4 の 14）。
pub(super) fn nested_locking_error(lc: &LockingClause) -> Error {
    Error::not_supported(format!(
        "FOR {} in a subquery is not supported yet",
        lc.strength.as_str()
    ))
    .with_span(lc.span)
}

fn tuple_mode(s: LockStrength) -> LockTupleMode {
    match s {
        LockStrength::Update => LockTupleMode::Exclusive,
        LockStrength::NoKeyUpdate => LockTupleMode::NoKeyExclusive,
        LockStrength::Share => LockTupleMode::Share,
        LockStrength::KeyShare => LockTupleMode::KeyShare,
    }
}

fn mode_words(m: LockTupleMode) -> &'static str {
    match m {
        LockTupleMode::Exclusive => "UPDATE",
        LockTupleMode::NoKeyExclusive => "NO KEY UPDATE",
        LockTupleMode::Share => "SHARE",
        LockTupleMode::KeyShare => "KEY SHARE",
    }
}

fn row_wait(w: LockWaitPolicy) -> RowWait {
    match w {
        LockWaitPolicy::Block => RowWait::Block,
        LockWaitPolicy::NoWait => RowWait::Error,
        LockWaitPolicy::SkipLocked => RowWait::Skip,
    }
}

/// 強い方が大きい（`Block < Skip < Error`。PostgreSQL の `LockWaitPolicy`）。
fn wait_rank(w: RowWait) -> u8 {
    match w {
        RowWait::Block => 0,
        RowWait::Skip => 1,
        RowWait::Error => 2,
    }
}

/// 25006（RW-D12）。解析後の問い合わせにロック対象があり、トランザクションが読み取り専用のときに
/// 呼び出し側（セッション）が返すエラー。ロック対象が無ければ `None`（`select 1 for update` は成功する）。
/// 文言は最初のロック対象の強度。
pub fn read_only_lock_error(q: &BoundQuery) -> Option<Error> {
    let BoundSetExpr::Select(sel) = &q.body else {
        return None;
    };
    let first = sel.locking.first()?;
    Some(Error::new(
        sqlstate::READ_ONLY_SQL_TRANSACTION,
        format!(
            "cannot execute SELECT FOR {} in a read-only transaction",
            mode_words(first.strength)
        ),
    ))
}

/// 生のパース木から見た、`FOR` のロック対象の表参照（`locked_table_refs` の要素）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableRefId {
    /// 書かれたスキーマ名（なければ `None`）。
    pub schema: Option<String>,
    /// 表名。
    pub table: String,
    /// 別名（なければ `None`）。
    pub alias: Option<String>,
}

impl TableRefId {
    /// `OF` で指す名前（別名があれば別名、なければ表名）。
    pub fn refname(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.table)
    }
}

/// 最上位の `FOR` 句を外した問い合わせと、外した句。最上位に `FOR` が無ければ `None`。
/// 外枠が ORDER BY / LIMIT / WITH を持たない括弧 `((select ... for update))` は中まで見る。
fn split_locking(q: &Query) -> Option<(Query, Vec<LockingClause>)> {
    if !q.locking.is_empty() {
        let mut stripped = q.clone();
        let clauses = std::mem::take(&mut stripped.locking);
        return Some((stripped, clauses));
    }
    if let QueryBody::Nested(inner) = &q.body
        && q.with.is_none()
        && q.order_by.is_empty()
        && q.limit.is_none()
        && q.offset.is_none()
    {
        let (stripped_inner, clauses) = split_locking(inner)?;
        let mut stripped = q.clone();
        stripped.body = QueryBody::Nested(Box::new(stripped_inner));
        return Some((stripped, clauses));
    }
    None
}

/// 生の最上位 SELECT から、`FOR` のロック対象になりうる表参照を FROM の出現順に返す（§6.4。LK が
/// リレーションロックの強度を決めるために呼ぶ。`FOR` の対象は `RowShare`、それ以外は `AccessShare`）。
/// 解析前なので 9〜14 のエラーは出さず、対象に入りうるものを返すだけ。`OF` なしは FROM の全ての表、
/// `OF` ありは名前が一致する表（同じ表参照は 1 度だけ）。副問い合わせ・関数は含まない。`FOR` が無い、
/// または本体が単純な SELECT でなければ空。
pub fn locked_table_refs(q: &Query) -> Vec<TableRefId> {
    let Some((stripped, clauses)) = split_locking(q) else {
        return Vec::new();
    };
    let mut body = &stripped.body;
    while let QueryBody::Nested(inner) = body {
        body = &inner.body;
    }
    let QueryBody::Select(sel) = body else {
        return Vec::new();
    };
    let mut refs = Vec::new();
    for item in &sel.from {
        collect_table_refs(item, &mut refs);
    }
    refs.into_iter()
        .filter(|r| {
            clauses.iter().any(|c| {
                c.of.is_empty()
                    || c.of
                        .iter()
                        .any(|n| n.parts.len() == 1 && n.name().value == r.refname())
            })
        })
        .collect()
}

fn collect_table_refs(item: &TableRef, out: &mut Vec<TableRefId>) {
    match item {
        TableRef::Table { name, alias, .. } => out.push(TableRefId {
            schema: name.schema().map(|s| s.value.clone()),
            table: name.name().value.clone(),
            alias: alias.as_ref().map(|a| a.name.value.clone()),
        }),
        TableRef::Join { left, right, .. } => {
            collect_table_refs(left, out);
            collect_table_refs(right, out);
        }
        TableRef::Function { .. } | TableRef::Subquery { .. } => {}
    }
}

/// 外部結合の NULL 側に当たる `Scan` の RTE を `out` に集める。
fn nullable_rtes(item: &FromItem, nullable: bool, out: &mut Vec<RteId>) {
    match item {
        FromItem::Scan(id) => {
            if nullable {
                out.push(*id);
            }
        }
        FromItem::Join {
            kind, left, right, ..
        } => {
            let left_null = nullable || matches!(kind, JoinType::Right | JoinType::Full);
            let right_null = nullable || matches!(kind, JoinType::Left | JoinType::Full);
            nullable_rtes(left, left_null, out);
            nullable_rtes(right, right_null, out);
        }
    }
}

impl Analyzer<'_> {
    /// 最上位の問い合わせ（`Statement::Query` と `INSERT ... SELECT` の SELECT）の解析。`FOR` 句を解決する。
    pub(super) fn analyze_top_query(&self, q: &Query, env: &QueryEnv<'_>) -> Result<BoundQuery> {
        let Some((stripped, clauses)) = split_locking(q) else {
            return self.analyze_query(q, env);
        };
        let mut bq = self.analyze_query(&stripped, env)?;
        let strength = clauses[0].strength.as_str();
        match &mut bq.body {
            BoundSetExpr::SetOp { .. } => {
                return Err(Error::not_supported(format!(
                    "FOR {strength} is not allowed with UNION/INTERSECT/EXCEPT"
                ))
                .with_span(clauses[0].span));
            }
            BoundSetExpr::Values { .. } => {
                return Err(Error::not_supported(format!(
                    "FOR {strength} cannot be applied to VALUES"
                ))
                .with_span(clauses[0].span));
            }
            BoundSetExpr::Select(sel) => {
                if matches!(
                    sel.rtable.as_slice(),
                    [rte] if rte.refname.as_deref() == Some("*VALUES*")
                ) {
                    return Err(Error::not_supported(format!(
                        "FOR {strength} cannot be applied to VALUES"
                    ))
                    .with_span(clauses[0].span));
                }
                sel.locking = resolve_locking(sel, &clauses)?;
            }
        }
        Ok(bq)
    }
}

/// 句を解決し、RTE ごとにまとめた `BoundSelect.locking` を作る（§6.4 の順に検査する）。
fn resolve_locking(
    sel: &BoundSelect,
    clauses: &[LockingClause],
) -> Result<Vec<BoundLockingClause>> {
    // RTE ごとの (最強の強度, 強い方の wait)。
    let mut marks: Vec<Option<(LockTupleMode, RowWait)>> = vec![None; sel.rtable.len()];
    for lc in clauses {
        check_select_locking(sel, lc)?;
        let mut targets = Vec::new();
        if lc.of.is_empty() {
            for (i, rte) in sel.rtable.iter().enumerate() {
                match &rte.kind {
                    RteKind::Table { .. } => targets.push(i),
                    RteKind::Subquery { .. } => return Err(subquery_error(lc)),
                    // JOIN・関数・VALUES・CTE の参照は黙って無視する。
                    _ => {}
                }
            }
        } else {
            for name in &lc.of {
                targets.push(resolve_of_name(sel, lc, name)?);
            }
        }
        for i in targets {
            let RteKind::Table { table } = &sel.rtable[i].kind else {
                continue;
            };
            match table.kind {
                RelKind::Sequence => {
                    return Err(Error::new(
                        sqlstate::WRONG_OBJECT_TYPE,
                        format!("cannot lock rows in sequence \"{}\"", table.name),
                    )
                    .with_span(lc.span));
                }
                // 仮想リレーション（pg_locks など）は行ロックの対象にしない（RW-K4）。
                RelKind::Virtual => continue,
                RelKind::Table | RelKind::Index => {}
            }
            let new = (tuple_mode(lc.strength), row_wait(lc.wait));
            marks[i] = Some(match marks[i] {
                None => new,
                Some((m, w)) => (
                    m.max(new.0),
                    if wait_rank(new.1) > wait_rank(w) {
                        new.1
                    } else {
                        w
                    },
                ),
            });
        }
    }

    // 外部結合の NULL 側（§6.4 の 12。PostgreSQL では計画時のエラー）。
    let mut nullable = Vec::new();
    for item in &sel.from {
        nullable_rtes(item, false, &mut nullable);
    }
    for (i, mark) in marks.iter().enumerate() {
        if let Some((mode, _)) = mark
            && nullable.contains(&RteId(u16::try_from(i).unwrap_or(u16::MAX)))
        {
            return Err(Error::not_supported(format!(
                "FOR {} cannot be applied to the nullable side of an outer join",
                mode_words(*mode)
            )));
        }
    }

    // RTE の昇順に、同じ (強度, wait) を 1 本の句にまとめる。
    let mut out: Vec<BoundLockingClause> = Vec::new();
    for (i, mark) in marks.iter().enumerate() {
        let Some((strength, wait)) = *mark else {
            continue;
        };
        let id =
            RteId(u16::try_from(i).map_err(|_| Error::internal("too many range table entries"))?);
        match out
            .iter_mut()
            .find(|c| c.strength == strength && c.wait == wait)
        {
            Some(c) => c.rtes.push(id),
            None => out.push(BoundLockingClause {
                strength,
                wait,
                rtes: vec![id],
            }),
        }
    }
    Ok(out)
}

/// §6.4 の 2〜7（PostgreSQL の `CheckSelectLocking`）。集合演算（1）は呼び出し側。
/// 目的リストのウィンドウ関数・集合を返す関数は、アナライザが先に別の 0A000 で拒否する。
fn check_select_locking(sel: &BoundSelect, lc: &LockingClause) -> Result<()> {
    let s = lc.strength.as_str();
    let what = if sel.distinct != super::bound::BoundDistinct::None {
        "DISTINCT clause"
    } else if !sel.group_by.is_empty() {
        "GROUP BY clause"
    } else if sel.having.is_some() {
        "HAVING clause"
    } else if sel.has_agg {
        "aggregate functions"
    } else {
        return Ok(());
    };
    Err(Error::not_supported(format!("FOR {s} is not allowed with {what}")).with_span(lc.span))
}

fn subquery_error(lc: &LockingClause) -> Error {
    Error::not_supported(format!(
        "FOR {} on a subquery in the FROM clause is not supported yet",
        lc.strength.as_str()
    ))
    .with_span(lc.span)
}

/// `OF name` を `rtable` の添字にする（§6.4 の 8〜10）。
fn resolve_of_name(sel: &BoundSelect, lc: &LockingClause, name: &ObjectName) -> Result<usize> {
    let s = lc.strength.as_str();
    if name.parts.len() > 1 {
        return Err(Error::syntax_at(
            name.span,
            format!("FOR {s} must specify unqualified relation names"),
        ));
    }
    let refname = &name.name().value;
    let Some(i) = sel
        .rtable
        .iter()
        .position(|r| r.refname.as_deref() == Some(refname.as_str()))
    else {
        return Err(Error::new(
            sqlstate::UNDEFINED_TABLE,
            format!("relation \"{refname}\" in FOR {s} clause not found in FROM clause"),
        )
        .with_span(name.span));
    };
    let cannot = |what: &str| {
        Error::not_supported(format!("FOR {s} cannot be applied to {what}")).with_span(name.span)
    };
    match &sel.rtable[i].kind {
        RteKind::Table { .. } => Ok(i),
        RteKind::Subquery { .. } => Err(subquery_error(lc)),
        RteKind::Function { .. } => Err(cannot("a function")),
        RteKind::Values { .. } => Err(cannot("VALUES")),
        RteKind::CteRef { .. } => Err(cannot("a WITH query")),
        RteKind::Join { .. } => Err(cannot("a join")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::bound::BoundStatement;
    use crate::analyzer::tests::{catalog, create, query, run};
    use crate::catalog::fake::FakeCatalog;

    fn cat() -> FakeCatalog {
        let mut c = catalog();
        create(&mut c, "CREATE TABLE a (x int, y int)");
        create(&mut c, "CREATE TABLE b (x int, z int)");
        c
    }

    fn locking(c: &FakeCatalog, sql: &str) -> Vec<BoundLockingClause> {
        match query(c, sql).body {
            BoundSetExpr::Select(s) => s.locking,
            other => panic!("not select: {other:?}"),
        }
    }

    #[track_caller]
    fn e(c: &FakeCatalog, sql: &str) -> (&'static str, String) {
        match run(c, sql) {
            Ok(s) => panic!("expected error for {sql}: {s:?}"),
            Err(e) => (e.sqlstate.code(), e.message),
        }
    }

    #[track_caller]
    fn expect(c: &FakeCatalog, sql: &str, state: &str, msg: &str) {
        assert_eq!(e(c, sql), (state, msg.to_owned()), "{sql}");
    }

    fn parsed(sql: &str) -> Query {
        match crate::sql::parse(sql).unwrap().remove(0) {
            crate::sql::ast::Statement::Query(q) => *q,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn strengths_and_waits() {
        let c = cat();
        for (sql, mode, wait) in [
            ("for update", LockTupleMode::Exclusive, RowWait::Block),
            (
                "for no key update nowait",
                LockTupleMode::NoKeyExclusive,
                RowWait::Error,
            ),
            ("for share skip locked", LockTupleMode::Share, RowWait::Skip),
            ("for key share", LockTupleMode::KeyShare, RowWait::Block),
        ] {
            let l = locking(&c, &format!("select * from a {sql}"));
            assert_eq!(
                l,
                vec![BoundLockingClause {
                    strength: mode,
                    wait,
                    rtes: vec![RteId(0)]
                }],
                "{sql}"
            );
        }
    }

    #[test]
    fn no_targets_is_ok() {
        let c = cat();
        assert!(locking(&c, "select 1 for update").is_empty());
        assert!(locking(&c, "select * from generate_series(1,2) g for update").is_empty());
        let l = locking(&c, "select * from a, generate_series(1,2) g for update");
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].rtes, vec![RteId(0)]);
    }

    #[test]
    fn of_alias_and_merge() {
        let c = cat();
        let l = locking(&c, "select * from a, b for update of b");
        assert_eq!(l[0].rtes, vec![RteId(1)]);
        let l = locking(&c, "select * from a as p, b for share of p");
        assert_eq!(l[0].rtes, vec![RteId(0)]);
        // 別の強度は別の句に、RTE ごとに 1 本。
        let l = locking(
            &c,
            "select * from a, b for update of a for share of b nowait",
        );
        assert_eq!(l.len(), 2);
        assert_eq!(
            (l[0].strength, l[0].wait),
            (LockTupleMode::Exclusive, RowWait::Block)
        );
        assert_eq!(
            (l[1].strength, l[1].wait),
            (LockTupleMode::Share, RowWait::Error)
        );
        // 同じ RTE は最強の強度・強い wait。
        let l = locking(
            &c,
            "select * from a for share nowait for update skip locked",
        );
        assert_eq!(l.len(), 1);
        assert_eq!(
            (l[0].strength, l[0].wait),
            (LockTupleMode::Exclusive, RowWait::Error)
        );
        let l = locking(
            &c,
            "select * from a for key share skip locked for no key update",
        );
        assert_eq!(
            (l[0].strength, l[0].wait),
            (LockTupleMode::NoKeyExclusive, RowWait::Skip)
        );
        // 強度と wait が同じ RTE は 1 本の句に集まる。
        let l = locking(&c, "select * from a, b for update");
        assert_eq!(l.len(), 1);
        assert_eq!(l[0].rtes, vec![RteId(0), RteId(1)]);
    }

    #[test]
    fn limit_and_order_by_ok() {
        let c = cat();
        assert_eq!(
            locking(&c, "select * from a order by x limit 1 for update").len(),
            1
        );
        assert_eq!(locking(&c, "select * from a for update limit 1").len(), 1);
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn section_6_4_errors() {
        let c = cat();
        let na = "0A000";
        expect(
            &c,
            "select x from a union select x from b for update",
            na,
            "FOR UPDATE is not allowed with UNION/INTERSECT/EXCEPT",
        );
        expect(
            &c,
            "select x from a except select x from b for share",
            na,
            "FOR SHARE is not allowed with UNION/INTERSECT/EXCEPT",
        );
        expect(
            &c,
            "select distinct x from a for update",
            na,
            "FOR UPDATE is not allowed with DISTINCT clause",
        );
        expect(
            &c,
            "select distinct on (x) x from a for no key update",
            na,
            "FOR NO KEY UPDATE is not allowed with DISTINCT clause",
        );
        expect(
            &c,
            "select x from a group by x for update",
            na,
            "FOR UPDATE is not allowed with GROUP BY clause",
        );
        expect(
            &c,
            "select 1 from a having true for key share",
            na,
            "FOR KEY SHARE is not allowed with HAVING clause",
        );
        expect(
            &c,
            "select count(*) from a for update",
            na,
            "FOR UPDATE is not allowed with aggregate functions",
        );
        expect(
            &c,
            "select * from a for update of public.a",
            "42601",
            "FOR UPDATE must specify unqualified relation names",
        );
        expect(
            &c,
            "select * from a for share of zz",
            "42P01",
            "relation \"zz\" in FOR SHARE clause not found in FROM clause",
        );
        expect(
            &c,
            "select * from a as p for update of a",
            "42P01",
            "relation \"a\" in FOR UPDATE clause not found in FROM clause",
        );
        expect(
            &c,
            "select * from a, generate_series(1,2) g for update of g",
            na,
            "FOR UPDATE cannot be applied to a function",
        );
        expect(
            &c,
            "select * from (values (1)) v(q) for update of v",
            na,
            "FOR UPDATE cannot be applied to VALUES",
        );
        expect(
            &c,
            "values (1) for update",
            na,
            "FOR UPDATE cannot be applied to VALUES",
        );
        expect(
            &c,
            "with w as (select 1 as q) select * from w for update of w",
            na,
            "FOR UPDATE cannot be applied to a WITH query",
        );
        expect(
            &c,
            "select * from a left join b on a.x = b.x for update of b",
            na,
            "FOR UPDATE cannot be applied to the nullable side of an outer join",
        );
        expect(
            &c,
            "select * from a right join b on a.x = b.x for share",
            na,
            "FOR SHARE cannot be applied to the nullable side of an outer join",
        );
        assert_eq!(
            locking(
                &c,
                "select * from a left join b on a.x = b.x for update of a"
            )
            .len(),
            1
        );
    }

    #[test]
    fn yuzhu_specific_errors() {
        let c = cat();
        let na = "0A000";
        expect(
            &c,
            "select * from (select * from a) s for update",
            na,
            "FOR UPDATE on a subquery in the FROM clause is not supported yet",
        );
        expect(
            &c,
            "select * from (select * from a) s for update of s",
            na,
            "FOR UPDATE on a subquery in the FROM clause is not supported yet",
        );
        // OF が別の表だけを指すなら可。
        let l = locking(&c, "select * from (select 1) s, a for update of a");
        assert_eq!(l[0].rtes.len(), 1);
        for sql in [
            "select * from (select * from a for update) s",
            "select * from a where x in (select x from b for update)",
            "with w as (select * from a for update) select * from w",
            "(select x from a for update) union select x from b",
        ] {
            assert_eq!(e(&c, sql).0, na, "{sql}");
            assert!(
                e(&c, sql).1.ends_with("in a subquery is not supported yet"),
                "{sql}"
            );
        }
    }

    #[test]
    fn insert_select_top_level_allowed() {
        let c = cat();
        match run(&c, "insert into b select x, y from a for update").unwrap() {
            BoundStatement::Insert(_) => {}
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn read_only_error() {
        let c = cat();
        let q = query(&c, "select * from a for key share");
        let err = read_only_lock_error(&q).unwrap();
        assert_eq!(err.sqlstate.code(), "25006");
        assert_eq!(
            err.message,
            "cannot execute SELECT FOR KEY SHARE in a read-only transaction"
        );
        assert!(read_only_lock_error(&query(&c, "select 1 for update")).is_none());
        assert!(read_only_lock_error(&query(&c, "select * from a")).is_none());
    }

    fn refs(sql: &str) -> Vec<(Option<String>, String, Option<String>)> {
        locked_table_refs(&parsed(sql))
            .into_iter()
            .map(|r| (r.schema, r.table, r.alias))
            .collect()
    }

    #[test]
    fn locked_table_refs_follow_the_clauses() {
        assert!(refs("select * from a").is_empty());
        assert!(refs("select 1 from a union select 1 from b").is_empty());
        let r = refs("select * from public.a p join b on true, generate_series(1,2) for update");
        assert_eq!(
            r,
            vec![
                (Some("public".into()), "a".into(), Some("p".into())),
                (None, "b".into(), None)
            ]
        );
        let r = refs("select * from a p, b for update of p");
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].1, "a");
        assert_eq!(
            refs("select * from a, b for update of a for share of b").len(),
            2
        );
        assert_eq!(refs("select * from a for update of zz").len(), 0);
        assert_eq!(refs("(select * from a for update)").len(), 1);
        assert_eq!(refs("select * from a for update of a, a").len(), 1);
    }
}
