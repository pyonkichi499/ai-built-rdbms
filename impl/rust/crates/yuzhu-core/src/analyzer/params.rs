//! `$n` パラメータの型推論（XQ-3。`m5/04` §6.4）。
//!
//! PostgreSQL の `parse_param.c`（`variable_paramref_hook` / `variable_coerce_param_hook`）と
//! `check_variable_parameters` に当たる。[`ParamCtx`] が各 `$n` の型を覚え、解析が unknown の
//! `$n` を型変換の対象にした時点で（`coerce_type` の H2 など）その型に確定する。
//! 解析の最後に [`ParamCtx::finish`] が、確定しなかった `$n`（42P18）と、確定済みの型と食い違う
//! 節点（42P08）を検査する。
//!
//! `Analyzer` は `&self` で動くので、状態は `RefCell` に持つ。

use std::cell::RefCell;

use super::Analyzer;
use super::bound::{BoundExpr, BoundExprKind, BoundStatement};
use super::coerce::{CoercionContext, tname};
use super::scope::ScopeStack;
use crate::catalog::{CatalogReader, builtin};
use crate::error::{Error, Result, Span, sqlstate};
use crate::sql::ast::{Expr, Statement};
use crate::types::{Oid, SqlType, oid};

/// 番号の上限（Bind のパラメータ数は 16 ビット）。
const MAX_PARAMS: u32 = 65_535;

#[derive(Debug, Default)]
struct State {
    /// `types[n - 1]`。`oid::UNKNOWN` は未確定。
    types: Vec<Oid>,
    /// `reference` が UNKNOWN を返し、`resolve` をまだ通っていない節点 `(位置, index)`。
    unresolved: Vec<(Span, u16)>,
}

/// `$n` の型の状態。1 回の解析（Parse または再解析）で 1 つ作る。
#[derive(Debug, Default)]
pub struct ParamCtx {
    state: RefCell<State>,
}

impl ParamCtx {
    /// `declared` は Parse メッセージの型の OID（0 = 未指定）。非 0 の OID が型表に無ければ
    /// `XX000 cache lookup failed for type N`、実装していない型なら `0A000`。
    pub fn new(declared: &[Oid], catalog: &dyn CatalogReader) -> Result<ParamCtx> {
        let mut types = Vec::with_capacity(declared.len());
        for &t in declared {
            if t == 0 {
                types.push(oid::UNKNOWN);
                continue;
            }
            if catalog.type_by_oid(t).is_none() {
                return Err(Error::internal(format!("cache lookup failed for type {t}")));
            }
            if !builtin::is_supported_type(t) {
                return Err(super::coerce::type_not_supported(t));
            }
            types.push(t);
        }
        Ok(ParamCtx {
            state: RefCell::new(State {
                types,
                unresolved: Vec::new(),
            }),
        })
    }

    /// `variable_paramref_hook`: `$n` の型（未確定なら `UNKNOWN`）を返す。`n == 0` と上限超えは
    /// `42P02`（位置つき）。UNKNOWN のときは `finish` の検査のために節点を覚える。
    pub(super) fn reference(&self, n: u32, span: Span) -> Result<Oid> {
        if n == 0 || n > MAX_PARAMS {
            return Err(Error::new(
                sqlstate::UNDEFINED_PARAMETER,
                format!("there is no parameter ${n}"),
            )
            .with_span(span));
        }
        let index = u16::try_from(n - 1).map_err(|_| Error::internal("parameter index"))?;
        let mut st = self.state.borrow_mut();
        if st.types.len() < usize::from(index) + 1 {
            st.types.resize(usize::from(index) + 1, oid::UNKNOWN);
        }
        let t = st.types[usize::from(index)];
        if t == oid::UNKNOWN && !st.unresolved.contains(&(span, index)) {
            st.unresolved.push((span, index));
        }
        Ok(t)
    }

    /// `variable_coerce_param_hook`: unknown の節点が `target` への型変換の対象になった。
    /// 未確定なら `target` に確定し、確定済みで `target` と違えば `42P08`。
    pub(super) fn resolve(&self, index: u16, target: Oid, span: Span) -> Result<()> {
        let mut st = self.state.borrow_mut();
        let slot = usize::from(index);
        if st.types.len() <= slot {
            st.types.resize(slot + 1, oid::UNKNOWN);
        }
        let cur = st.types[slot];
        if cur == oid::UNKNOWN {
            st.types[slot] = target;
        } else if cur != target {
            return Err(Error::new(
                sqlstate::AMBIGUOUS_PARAMETER,
                format!(
                    "inconsistent types deduced for parameter ${}",
                    u32::from(index) + 1
                ),
            )
            .with_detail(format!("{} versus {}", tname(cur), tname(target)))
            .with_span(span));
        }
        st.unresolved.retain(|(s, i)| !(*s == span && *i == index));
        Ok(())
    }

    /// 現在の型の一覧（未確定は `UNKNOWN`）。
    pub fn types(&self) -> Vec<Oid> {
        self.state.borrow().types.clone()
    }

    /// `check_variable_parameters` と、未確定の検査。確定した型の一覧を返す。
    pub fn finish(&self) -> Result<Vec<Oid>> {
        let st = self.state.borrow();
        // (b) UNKNOWN のまま残った節点のうち、同じ番号がほかの出現で確定しているもの。
        let mut stale: Vec<&(Span, u16)> = st
            .unresolved
            .iter()
            .filter(|(_, i)| st.types[usize::from(*i)] != oid::UNKNOWN)
            .collect();
        stale.sort_by_key(|(s, _)| s.start);
        if let Some((span, i)) = stale.first() {
            return Err(Error::new(
                sqlstate::AMBIGUOUS_PARAMETER,
                format!(
                    "could not determine data type of parameter ${}",
                    u32::from(*i) + 1
                ),
            )
            .with_span(*span));
        }
        if let Some(n) = st.types.iter().position(|t| *t == oid::UNKNOWN) {
            return Err(Error::new(
                sqlstate::INDETERMINATE_DATATYPE,
                format!("could not determine data type of parameter ${}", n + 1),
            ));
        }
        Ok(st.types.clone())
    }
}

/// `$n` を含む文を解析する。`declared` は Parse の型（0 = 未指定）。戻り値は確定した型の一覧
/// （長さは `max(宣言の数, 使われた最大の n)`）。
pub fn analyze_with_params(
    stmt: &Statement,
    catalog: &dyn CatalogReader,
    declared: &[Oid],
) -> Result<(BoundStatement, Vec<Oid>)> {
    let ctx = ParamCtx::new(declared, catalog)?;
    let a = Analyzer {
        catalog,
        params: Some(&ctx),
    };
    let bound = a.analyze_statement(stmt)?;
    let types = ctx.finish()?;
    if cfg!(debug_assertions) {
        bound.validate()?;
    }
    Ok((bound, types))
}

/// `EXECUTE name (e1, e2, ...)` の `index` 番目の引数（0 始まり）を解析し、assignment の文脈で
/// `target` の型へ強制変換する。変換できなければ `42804`。副問い合わせは `0A000`、集約は `42803`。
pub fn analyze_execute_param(
    e: &Expr,
    target: Oid,
    index: usize,
    catalog: &dyn CatalogReader,
) -> Result<BoundExpr> {
    let a = Analyzer {
        catalog,
        params: None,
    };
    let scopes = ScopeStack::empty();
    let cx = super::expr::ExprCtx::new(&scopes, super::scope::ParseExprKind::Values);
    let b = a.transform_expr(e, &cx)?;
    if b.contains_aggregate() {
        return Err(Error::new(
            sqlstate::GROUPING_ERROR,
            "aggregate functions are not allowed in EXECUTE parameters",
        )
        .with_span(b.span));
    }
    let (src, span) = (b.ty.oid, b.span);
    a.coerce_to_target_type(b, SqlType::of(target), CoercionContext::Assignment)?
        .ok_or_else(|| {
            Error::new(
                sqlstate::DATATYPE_MISMATCH,
                format!(
                    "parameter ${} of type {} cannot be coerced to the expected type {}",
                    index + 1,
                    tname(src),
                    tname(target)
                ),
            )
            .with_hint("You will need to rewrite or cast the expression.")
            .with_span(span)
        })
}

/// 式が unknown 型の `$n` か（H2 / H3 / H4 の判定）。
pub(super) fn extern_param_index(e: &BoundExpr) -> Option<u16> {
    match e.kind {
        BoundExprKind::ExternParam(i) if e.ty.oid == oid::UNKNOWN => Some(i),
        _ => None,
    }
}

impl Analyzer<'_> {
    /// H2: unknown の `$n` を `target` に確定し、節点の型だけを書き換える（キャストは挟まない）。
    pub(super) fn coerce_extern_param(&self, mut e: BoundExpr, target: Oid) -> Result<BoundExpr> {
        let Some(i) = extern_param_index(&e) else {
            return Ok(e);
        };
        if let Some(ctx) = self.params {
            ctx.resolve(i, target, e.span)?;
        }
        e.ty = SqlType::of(target);
        Ok(e)
    }

    /// H3: 出力列の unknown の `$n` を text にする（`resolveTargetListUnknowns`）。
    pub(super) fn resolve_unknown(&self, e: BoundExpr) -> Result<BoundExpr> {
        if extern_param_index(&e).is_some() {
            return self.coerce_extern_param(e, oid::TEXT);
        }
        Ok(super::coerce::resolve_unknown(e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::analyzer::tests::{create, run};
    use crate::catalog::fake::FakeCatalog;
    use crate::types::oid::{BOOL, FLOAT8, INT4, INT8, NUMERIC, TEXT, TIMESTAMP, UNKNOWN, VARCHAR};

    const BPCHAR: Oid = 1042;
    const INTERVAL: Oid = 1186;

    fn cat() -> FakeCatalog {
        let mut c = FakeCatalog::new("postgres");
        create(
            &mut c,
            "CREATE TABLE t (id bigint, a int, b text, c varchar(10), d numeric(10,2), \
             e bool, f float8, g timestamp)",
        );
        c
    }

    fn infer(c: &FakeCatalog, sql: &str, declared: &[Oid]) -> Result<Vec<Oid>> {
        let stmts = crate::sql::parse(sql)?;
        analyze_with_params(&stmts[0], c, declared).map(|(_, t)| t)
    }

    #[track_caller]
    fn ok(c: &FakeCatalog, sql: &str, declared: &[Oid], expect: &[Oid]) {
        match infer(c, sql, declared) {
            Ok(t) => assert_eq!(t, expect, "{sql}"),
            Err(e) => panic!("{sql}: unexpected error {e:?}"),
        }
    }

    #[track_caller]
    fn fails(c: &FakeCatalog, sql: &str, declared: &[Oid], code: &str, msg: &str) -> Error {
        match infer(c, sql, declared) {
            Ok(t) => panic!("{sql}: expected {code}, got {t:?}"),
            Err(e) => {
                assert_eq!(e.sqlstate.code(), code, "{sql}: {e:?}");
                assert!(e.message.contains(msg), "{sql}: {e:?}");
                e
            }
        }
    }

    /// (SQL, 期待する型の一覧)。期待は PostgreSQL 17 の `ParameterDescription`。
    const OK_CASES: &[(&str, &[Oid])] = &[
        ("SELECT * FROM t WHERE id = $1", &[INT8]),
        ("SELECT $1 = $2", &[TEXT, TEXT]),
        ("SELECT $1 = 1 AND $1 = 1.5", &[INT4]),
        ("SELECT * FROM t WHERE a = $1 AND f = $1", &[INT4]),
        ("SELECT $1 = 1 AND $1 IS NULL", &[INT4]),
        ("SELECT $1 + 1", &[INT4]),
        ("SELECT 1 + $1", &[INT4]),
        ("SELECT $1 * 2.5", &[NUMERIC]),
        ("SELECT -$1::int", &[INT4]),
        ("SELECT g - $1 FROM t", &[TIMESTAMP]),
        ("INSERT INTO t(id, c) VALUES ($1, $2)", &[INT8, VARCHAR]),
        ("INSERT INTO t(id, a) SELECT $1, $2", &[INT8, INT4]),
        (
            "INSERT INTO t(id, a) VALUES ($1, $2), ($3, 5)",
            &[INT8, INT4, INT8],
        ),
        ("INSERT INTO t(id) VALUES ($1 + 1)", &[INT4]),
        ("UPDATE t SET c = $1 WHERE id = $2", &[VARCHAR, INT8]),
        ("UPDATE t SET d = $1 * 2 WHERE id = $2", &[INT4, INT8]),
        ("DELETE FROM t WHERE id = $1", &[INT8]),
        ("DELETE FROM t WHERE b = $1 AND a > $2", &[TEXT, INT4]),
        ("SELECT * FROM t LIMIT $1 OFFSET $2", &[INT8, INT8]),
        ("SELECT * FROM t FETCH FIRST $1 ROWS ONLY", &[INT8]),
        ("SELECT * FROM t LIMIT $1::int", &[INT4]),
        ("SELECT * FROM t WHERE id = $1 LIMIT $2", &[INT8, INT8]),
        ("SELECT $1::int4", &[INT4]),
        ("SELECT $1::numeric(4,1)", &[NUMERIC]),
        ("SELECT CAST($1 AS char(3))", &[BPCHAR]),
        ("SELECT $1::int, $1::bigint", &[INT4]),
        ("SELECT $1::int8 + $1::int4", &[INT8]),
        ("SELECT $1", &[TEXT]),
        ("SELECT $1 AS x", &[TEXT]),
        ("SELECT ($1)", &[TEXT]),
        ("VALUES ($1)", &[TEXT]),
        ("SELECT $1 ORDER BY 1", &[TEXT]),
        ("SELECT DISTINCT $1", &[TEXT]),
        ("SELECT $1 LIMIT 1", &[TEXT]),
        ("SELECT * FROM t ORDER BY $1", &[TEXT]),
        ("SELECT * FROM t ORDER BY a + $1", &[INT4]),
        ("SELECT $1::int4, $1", &[INT4]),
        ("SELECT * FROM t WHERE b LIKE $1", &[TEXT]),
        ("SELECT $1 ~ 'a'", &[TEXT]),
        ("SELECT $1 LIKE 'a%'", &[TEXT]),
        ("SELECT * FROM t WHERE id IN ($1, $2)", &[INT8, INT8]),
        ("SELECT $1 IN (1, 2)", &[INT4]),
        ("SELECT $1 IN ('a','b')", &[TEXT]),
        ("SELECT $1 IN ($2, $3)", &[TEXT, TEXT, TEXT]),
        ("SELECT * FROM t WHERE a BETWEEN $1 AND $2", &[INT4, INT4]),
        ("SELECT $1 BETWEEN 1 AND 2", &[INT4]),
        ("SELECT * FROM t WHERE a IS DISTINCT FROM $1", &[INT4]),
        ("SELECT $1 IS DISTINCT FROM 1", &[INT4]),
        ("SELECT length($1)", &[TEXT]),
        ("SELECT upper($1)", &[TEXT]),
        ("SELECT * FROM t WHERE lower($1) = $2", &[TEXT, TEXT]),
        ("SELECT substring($1, 1, 2)", &[TEXT]),
        ("SELECT substring($1 from $2)", &[TEXT, TEXT]),
        ("SELECT position($1 in 'abc')", &[TEXT]),
        ("SELECT abs($1)", &[FLOAT8]),
        ("SELECT round($1)", &[FLOAT8]),
        ("SELECT power($1, 2)", &[FLOAT8]),
        ("SELECT $1 ^ 2", &[FLOAT8]),
        ("SELECT round($1, 2)", &[NUMERIC]),
        ("SELECT coalesce($1, 1)", &[INT4]),
        ("SELECT NULLIF($1, 1)", &[INT4]),
        ("SELECT greatest($1, 2)", &[INT4]),
        ("SELECT coalesce($1::int, $2)", &[INT4, INT4]),
        ("SELECT coalesce($1, $2)", &[TEXT, TEXT]),
        ("SELECT * FROM generate_series(1, $1)", &[INT4]),
        (
            "SELECT CASE WHEN $1 THEN $2 ELSE $3 END",
            &[BOOL, TEXT, TEXT],
        ),
        ("SELECT CASE WHEN true THEN $1 ELSE 1 END, $1", &[INT4]),
        ("SELECT * FROM t WHERE $1", &[BOOL]),
        ("SELECT * FROM t WHERE e AND $1", &[BOOL]),
        ("SELECT * FROM t WHERE NOT $1", &[BOOL]),
        ("SELECT $1 OR $2", &[BOOL, BOOL]),
        ("SELECT $1 UNION SELECT 1", &[INT4]),
        ("SELECT a FROM t UNION ALL SELECT $1", &[INT4]),
        ("SELECT (SELECT $1)", &[TEXT]),
        ("SELECT sum(a + $1) FROM t", &[INT4]),
        ("SELECT count(*) FROM t HAVING count(*) > $1", &[INT8]),
        ("SELECT count(*) FROM t GROUP BY $1", &[TEXT]),
        ("SELECT max($1) FROM t", &[TEXT]),
        ("SELECT 'a' || $1", &[TEXT]),
        ("SELECT $1 || 'a'", &[TEXT]),
        ("SELECT 1 || $1", &[TEXT]),
        ("SELECT $1 || $2", &[TEXT, TEXT]),
        ("SELECT * FROM t WHERE c = $1", &[TEXT]),
        ("SELECT * FROM t WHERE d = $1", &[NUMERIC]),
        ("SELECT * FROM t WHERE g = $1", &[TIMESTAMP]),
        ("SELECT * FROM t WHERE e = $1", &[BOOL]),
        ("SELECT * FROM t WHERE f > $1", &[FLOAT8]),
        ("SELECT * FROM t WHERE b || $1 = 'x'", &[TEXT]),
        (
            "SELECT pg_isolation_test_session_is_blocked($1, '{1,2}')",
            &[INT4],
        ),
        ("SELECT int4($1)", &[INT4]),
    ];

    #[test]
    fn inferred_types_match_postgres() {
        let c = cat();
        for (sql, expect) in OK_CASES {
            ok(&c, sql, &[], expect);
        }
    }

    /// (SQL, SQLSTATE, メッセージの一部)
    const ERR_CASES: &[(&str, &str, &str)] = &[
        ("SELECT $1 IS NULL", "42P18", "parameter $1"),
        ("SELECT $1 IS NOT NULL", "42P18", "parameter $1"),
        (
            "SELECT $1 IS NULL AND $1 = 1",
            "42P08",
            "could not determine data type of parameter $1",
        ),
        (
            "SELECT * FROM t WHERE $1 IS NULL OR a = $1",
            "42P08",
            "parameter $1",
        ),
        ("SELECT $1 + $2", "42725", "operator is not unique"),
        ("SELECT -$1", "42725", "operator is not unique"),
        ("SELECT -$1 + 1", "42725", "operator is not unique"),
        (
            "SELECT $1, $1::int4",
            "42P08",
            "inconsistent types deduced for parameter $1",
        ),
        (
            "SELECT $1 FROM t WHERE a = $1",
            "42P08",
            "inconsistent types deduced",
        ),
        (
            "SELECT CASE $1 WHEN 1 THEN 'a' ELSE 'b' END",
            "42883",
            "operator does not exist",
        ),
        (
            "SELECT * FROM t WHERE a = (SELECT $1)",
            "42883",
            "operator does not exist",
        ),
        (
            "SELECT * FROM t WHERE a IN (SELECT $1)",
            "42883",
            "operator does not exist",
        ),
        ("SELECT $0", "42P02", "there is no parameter $0"),
        ("SELECT $2", "42P18", "parameter $1"),
        ("SELECT $1, $3", "42P18", "parameter $2"),
        ("SELECT count($1) FROM t", "42P18", "parameter $1"),
    ];

    #[test]
    fn inference_errors_match_postgres() {
        let c = cat();
        for (sql, code, msg) in ERR_CASES {
            fails(&c, sql, &[], code, msg);
        }
    }

    #[test]
    fn conflict_detail_and_position() {
        let c = cat();
        let sql = "SELECT $1, $1::int4";
        let e = fails(
            &c,
            sql,
            &[],
            "42P08",
            "inconsistent types deduced for parameter $1",
        );
        assert_eq!(e.detail.as_deref(), Some("integer versus text"));
        assert_eq!(e.cursor_byte, Some(7));
        let sql = "SELECT $1 FROM t WHERE a = $1";
        let e = fails(&c, sql, &[], "42P08", "inconsistent");
        assert_eq!(e.detail.as_deref(), Some("integer versus text"));
        assert_eq!(e.cursor_byte, Some(7));
        // (b): 位置は IS NULL の `$1`。
        let sql = "SELECT $1 IS NULL AND $1 = 1";
        assert_eq!(
            fails(&c, sql, &[], "42P08", "could not").cursor_byte,
            Some(7)
        );
        // 42P18 は位置を持たない。
        assert!(
            fails(&c, "SELECT $1 IS NULL", &[], "42P18", "")
                .cursor_byte
                .is_none()
        );
    }

    #[test]
    fn declared_types() {
        let c = cat();
        ok(&c, "SELECT $1 + 1", &[INT8], &[INT8]);
        fails(
            &c,
            "SELECT $1 + 1",
            &[TEXT],
            "42883",
            "operator does not exist: text + integer",
        );
        ok(&c, "SELECT $1::int4", &[TEXT], &[TEXT]);
        ok(&c, "SELECT $1", &[0, INT4], &[TEXT, INT4]);
        fails(&c, "SELECT 1", &[0], "42P18", "parameter $1");
        fails(&c, "SELECT $1::int", &[0, 0, TEXT], "42P18", "parameter $2");
        fails(
            &c,
            "SELECT $1",
            &[99999],
            "XX000",
            "cache lookup failed for type 99999",
        );
        // 宣言どうしの 42P08 は起きない（確定済みの節点は unknown ではない）。
        ok(&c, "SELECT $1, $1::int4", &[TEXT], &[TEXT]);
    }

    #[test]
    fn simple_query_rejects_parameters() {
        let c = cat();
        let e = run(&c, "SELECT $1").unwrap_err();
        assert_eq!(e.sqlstate.code(), "42P02");
        assert!(e.cursor_byte.is_some());
        let e = run(&c, "CREATE TABLE q (a int DEFAULT $1)").unwrap_err();
        assert_eq!(e.sqlstate.code(), "42P02");
        let e = run(&c, "CREATE TABLE q (a int CHECK (a > $1))").unwrap_err();
        assert_eq!(e.sqlstate.code(), "42P02");
    }

    #[test]
    fn param_ctx_state_machine() {
        let c = cat();
        let ctx = ParamCtx::new(&[], &c).unwrap();
        let s = |n| Span::new(n, n + 2);
        assert_eq!(ctx.reference(1, s(0)).unwrap(), UNKNOWN);
        assert_eq!(ctx.finish().unwrap_err().sqlstate.code(), "42P18");
        ctx.resolve(0, INT4, s(0)).unwrap();
        assert_eq!(ctx.finish().unwrap(), vec![INT4]);
        // 確定済みの型で節点が作られる
        assert_eq!(ctx.reference(1, s(10)).unwrap(), INT4);
        // 衝突
        let e = ctx.resolve(0, TEXT, s(20)).unwrap_err();
        assert_eq!(e.sqlstate.code(), "42P08");
        assert_eq!(e.detail.as_deref(), Some("integer versus text"));
        // 同じ型なら一致
        ctx.resolve(0, INT4, s(20)).unwrap();
        // (b): UNKNOWN の節点が resolve を通らないまま、同じ番号が確定している
        let ctx = ParamCtx::new(&[], &c).unwrap();
        assert_eq!(ctx.reference(1, s(0)).unwrap(), UNKNOWN);
        assert_eq!(ctx.reference(1, s(5)).unwrap(), UNKNOWN);
        ctx.resolve(0, INT4, s(5)).unwrap();
        let e = ctx.finish().unwrap_err();
        assert_eq!(e.sqlstate.code(), "42P08");
        assert_eq!(e.cursor_byte, Some(0));
        // 番号 0 と範囲外
        assert_eq!(ctx.reference(0, s(0)).unwrap_err().sqlstate.code(), "42P02");
        assert_eq!(
            ctx.reference(70000, s(0)).unwrap_err().sqlstate.code(),
            "42P02"
        );
    }

    #[test]
    fn hooks_one_by_one() {
        let c = cat();
        let stmt = |sql: &str| crate::sql::parse(sql).unwrap().remove(0);
        // H2: キャストを挟まず、節点の型が書き換わる
        let ctx = ParamCtx::new(&[], &c).unwrap();
        let a = Analyzer {
            catalog: &c,
            params: Some(&ctx),
        };
        let p = BoundExpr::new(
            BoundExprKind::ExternParam(0),
            SqlType::UNKNOWN,
            Span::new(0, 2),
        );
        let out = a
            .coerce_type(p, INT8, CoercionContext::Implicit)
            .unwrap()
            .unwrap();
        assert!(matches!(out.kind, BoundExprKind::ExternParam(0)));
        assert_eq!(out.ty.oid, INT8);
        assert_eq!(ctx.types(), vec![INT8]);
        // H3: resolve_unknown が通る
        let ctx = ParamCtx::new(&[], &c).unwrap();
        let a = Analyzer {
            catalog: &c,
            params: Some(&ctx),
        };
        let p = BoundExpr::new(
            BoundExprKind::ExternParam(1),
            SqlType::UNKNOWN,
            Span::new(0, 2),
        );
        assert_eq!(a.resolve_unknown(p).unwrap().ty.oid, TEXT);
        assert_eq!(ctx.types(), vec![UNKNOWN, TEXT]);
        // 型の木が ExternParam で、INSERT ... SELECT が列の型になる（H6）
        let (b, t) =
            analyze_with_params(&stmt("INSERT INTO t(id, a) SELECT $1, $2"), &c, &[]).unwrap();
        assert_eq!(t, vec![INT8, INT4]);
        drop(b);
    }

    /// `timestamp + $1` は interval になる。interval が使える型になる（`is_supported_type`）までは確かめない
    /// （演算子表は結合済み。型の有効化は TD の残り）。
    #[test]
    fn timestamp_plus_param_is_interval_once_tables_are_merged() {
        let c = cat();
        let merged = crate::catalog::builtin::operators_named("+")
            .iter()
            .any(|o| o.left == Some(TIMESTAMP) && o.right == INTERVAL);
        if merged && crate::catalog::builtin::is_supported_type(INTERVAL) {
            ok(&c, "SELECT g + $1 FROM t", &[], &[INTERVAL]);
        }
    }

    #[test]
    fn reanalysis_with_declared_types_builds_the_same_tree() {
        let c = cat();
        for (sql, expect) in OK_CASES {
            let stmts = crate::sql::parse(sql).unwrap();
            let (first, types) = analyze_with_params(&stmts[0], &c, &[]).unwrap();
            let (again, types2) = analyze_with_params(&stmts[0], &c, &types).unwrap();
            assert_eq!(&types2, expect, "{sql}");
            assert_eq!(format!("{first:?}"), format!("{again:?}"), "{sql}");
        }
    }

    #[test]
    fn execute_param_coercion() {
        let c = cat();
        let e = crate::sql::parse("SELECT 1 + 2").unwrap();
        let crate::sql::ast::Statement::Query(q) = &e[0] else {
            panic!()
        };
        let _ = q;
        let expr = |s: &str| {
            let st = crate::sql::parse(&format!("SELECT {s}")).unwrap().remove(0);
            let crate::sql::ast::Statement::Query(q) = st else {
                panic!()
            };
            let crate::sql::ast::QueryBody::Select(sel) = q.body else {
                panic!()
            };
            let crate::sql::ast::SelectItem::Expr { expr, .. } = sel.targets[0].clone() else {
                panic!()
            };
            expr
        };
        let b = analyze_execute_param(&expr("1 + 2"), INT8, 0, &c).unwrap();
        assert_eq!(b.ty.oid, INT8);
        let e = analyze_execute_param(&expr("1"), BOOL, 0, &c).unwrap_err();
        assert_eq!(e.sqlstate.code(), "42804");
        assert_eq!(
            e.message,
            "parameter $1 of type integer cannot be coerced to the expected type boolean"
        );
        let e = analyze_execute_param(&expr("'x'"), INT4, 0, &c).unwrap_err();
        assert_eq!(e.sqlstate.code(), "22P02");
    }
}
