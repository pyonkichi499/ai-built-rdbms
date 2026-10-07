//! プランの印字（`plan_golden` とデバッグ用。`m4/04` §11.1）。
//!
//! 区画は 2 つ。論理の印字は L1、物理の印字は L2 が、自分の区画の中だけを編集する。
//! P0-e では `Debug` 出力を返す署名だけを置く。

use super::logical::LogicalQuery;
use super::physical::PhysicalQuery;

// ===== region: 論理（L1） =====

/// 論理プランを 1 ノード 1 行で印字する（`m4/04` §11.1）。子は 2 スペース下げる。列は `#<ColId>:<修飾名>`。
/// 式の中の副問い合わせは、式に `{SubLink ..}` と書き、そのノードの子として `~ ` で始まる行で続ける。
/// 共有する CTE は、本体の後に `CTE <名前> [#<添字>] output=[..]` と本体を続ける。
pub fn print_logical(q: &LogicalQuery) -> String {
    logical_print::print(q)
}

mod logical_print {
    use std::fmt::Write as _;

    use crate::expr::{BoolTestKind, ColId, ExprKind};
    use crate::planner::logical::{
        ColumnArena, JoinKind, LAggCall, LExpr, LogicalCte, LogicalPlan, LogicalQuery,
        LogicalSubquery,
    };
    use crate::types::{Datum, SqlType, format_type, io};

    pub(super) fn print(q: &LogicalQuery) -> String {
        let p = P {
            arena: &q.arena,
            ctes: &q.ctes,
        };
        let mut out = String::new();
        p.plan(&q.plan, 0, &mut out);
        for (i, c) in q.ctes.iter().enumerate() {
            let _ = writeln!(
                out,
                "CTE {} [#{i}] output={}{}",
                c.name,
                p.cols(&c.output),
                if c.inline { " inline" } else { "" }
            );
            p.plan(&c.plan, 1, &mut out);
        }
        out
    }

    struct P<'a> {
        arena: &'a ColumnArena,
        ctes: &'a [LogicalCte],
    }

    fn lit(d: &Datum, ty: SqlType) -> String {
        match d {
            Datum::Null => "NULL".to_owned(),
            Datum::Bool(b) => b.to_string(),
            Datum::Int2(_) | Datum::Int4(_) | Datum::Int8(_) | Datum::Numeric(_) => {
                io::output_text(d, ty).unwrap_or_default()
            }
            Datum::Text(s) | Datum::BpChar(s) => format!("'{}'", s.replace('\'', "''")),
            other => match io::output_text(other, ty) {
                Some(s) => format!("'{}'::{}", s.replace('\'', "''"), format_type(ty)),
                None => format!("{other:?}::{}", format_type(ty)),
            },
        }
    }

    impl<'a> P<'a> {
        fn col(&self, c: ColId) -> String {
            match self.arena.try_get(c) {
                Some(i) => match &i.qualifier {
                    Some(q) => format!("#{}:{q}.{}", c.0, i.name),
                    None => format!("#{}:{}", c.0, i.name),
                },
                None => format!("#{}", c.0),
            }
        }

        fn cols(&self, cs: &[ColId]) -> String {
            let v: Vec<String> = cs.iter().map(|c| self.col(*c)).collect();
            format!("[{}]", v.join(" "))
        }

        /// `(id, 式)` の並び。パススルーは `#id:名前`、それ以外は `#id := 式`。
        fn defs(&self, v: &'a [(ColId, LExpr)], subs: &mut Vec<&'a LogicalSubquery>) -> String {
            let items: Vec<String> = v
                .iter()
                .map(|(id, e)| match &e.kind {
                    ExprKind::Column(c) if c == id => self.col(*id),
                    _ => format!("#{} := {}", id.0, self.expr(e, subs)),
                })
                .collect();
            format!("[{}]", items.join(" "))
        }

        fn args(&self, es: &'a [LExpr], subs: &mut Vec<&'a LogicalSubquery>) -> String {
            es.iter()
                .map(|e| self.expr(e, subs))
                .collect::<Vec<_>>()
                .join(", ")
        }

        fn agg(&self, a: &'a LAggCall, subs: &mut Vec<&'a LogicalSubquery>) -> String {
            let args = if a.args.is_empty() {
                "*".to_owned()
            } else {
                self.args(&a.args, subs)
            };
            let mut s = format!(
                "{}({}{args})",
                a.func.name,
                if a.distinct { "DISTINCT " } else { "" }
            );
            if !a.order_by.is_empty() {
                let keys: Vec<String> = a
                    .order_by
                    .iter()
                    .map(|k| {
                        format!(
                            "{}{}",
                            self.expr(&k.expr, subs),
                            if k.descending { " DESC" } else { "" }
                        )
                    })
                    .collect();
                s.pop();
                let _ = write!(s, " ORDER BY {})", keys.join(", "));
            }
            if !a.order_by.is_empty() {
                let keys: Vec<String> = a
                    .order_by
                    .iter()
                    .map(|k| {
                        let desc = if k.descending { " DESC" } else { "" };
                        format!("{}{desc}", self.expr(&k.expr, subs))
                    })
                    .collect();
                s.pop();
                let _ = write!(s, " ORDER BY {})", keys.join(", "));
            }
            if let Some(f) = &a.filter {
                let _ = write!(s, " FILTER (WHERE {})", self.expr(f, subs));
            }
            s
        }

        #[allow(clippy::too_many_lines)]
        fn expr(&self, e: &'a LExpr, subs: &mut Vec<&'a LogicalSubquery>) -> String {
            match &e.kind {
                ExprKind::Literal(d) => lit(d, e.ty),
                ExprKind::Column(c) => self.col(*c),
                ExprKind::Operator { op, args } => match args.as_slice() {
                    [a] => format!("({} {})", op.name, self.expr(a, subs)),
                    [a, b] => format!(
                        "({} {} {})",
                        self.expr(a, subs),
                        op.name,
                        self.expr(b, subs)
                    ),
                    _ => format!("{}({})", op.name, self.args(args, subs)),
                },
                ExprKind::Function { func, args } => {
                    format!("{}({})", func.name, self.args(args, subs))
                }
                ExprKind::Cast { expr, .. } => {
                    format!("CAST({} AS {})", self.expr(expr, subs), format_type(e.ty))
                }
                ExprKind::CoerceTypmod { expr, .. } => {
                    format!("TYPMOD({} AS {})", self.expr(expr, subs), format_type(e.ty))
                }
                ExprKind::And(args) => format!(
                    "({})",
                    args.iter()
                        .map(|a| self.expr(a, subs))
                        .collect::<Vec<_>>()
                        .join(" AND ")
                ),
                ExprKind::Or(args) => format!(
                    "({})",
                    args.iter()
                        .map(|a| self.expr(a, subs))
                        .collect::<Vec<_>>()
                        .join(" OR ")
                ),
                ExprKind::Not(x) => format!("(NOT {})", self.expr(x, subs)),
                ExprKind::IsNull(x) => format!("({} IS NULL)", self.expr(x, subs)),
                ExprKind::IsNotNull(x) => format!("({} IS NOT NULL)", self.expr(x, subs)),
                ExprKind::BoolTest { expr, test } => {
                    let t = match test {
                        BoolTestKind::IsTrue => "IS TRUE",
                        BoolTestKind::IsNotTrue => "IS NOT TRUE",
                        BoolTestKind::IsFalse => "IS FALSE",
                        BoolTestKind::IsNotFalse => "IS NOT FALSE",
                        BoolTestKind::IsUnknown => "IS UNKNOWN",
                        BoolTestKind::IsNotUnknown => "IS NOT UNKNOWN",
                    };
                    format!("({} {t})", self.expr(expr, subs))
                }
                ExprKind::Case { arms, else_result } => {
                    let mut s = "CASE".to_owned();
                    for (c, r) in arms {
                        let _ = write!(
                            s,
                            " WHEN {} THEN {}",
                            self.expr(c, subs),
                            self.expr(r, subs)
                        );
                    }
                    if let Some(x) = else_result {
                        let _ = write!(s, " ELSE {}", self.expr(x, subs));
                    }
                    s + " END"
                }
                ExprKind::Coalesce(args) => format!("COALESCE({})", self.args(args, subs)),
                ExprKind::NullIf { left, right, .. } => format!(
                    "NULLIF({}, {})",
                    self.expr(left, subs),
                    self.expr(right, subs)
                ),
                ExprKind::DistinctFrom {
                    left,
                    right,
                    negated,
                    ..
                } => format!(
                    "({} IS {}DISTINCT FROM {})",
                    self.expr(left, subs),
                    if *negated { "NOT " } else { "" },
                    self.expr(right, subs)
                ),
                ExprKind::MinMax { greatest, args, .. } => format!(
                    "{}({})",
                    if *greatest { "GREATEST" } else { "LEAST" },
                    self.args(args, subs)
                ),
                ExprKind::Like {
                    expr,
                    pattern,
                    escape,
                    negated,
                    case_insensitive,
                } => {
                    let mut s = format!(
                        "({} {}{} {}",
                        self.expr(expr, subs),
                        if *negated { "NOT " } else { "" },
                        if *case_insensitive { "ILIKE" } else { "LIKE" },
                        self.expr(pattern, subs)
                    );
                    if let Some(x) = escape {
                        let _ = write!(s, " ESCAPE {}", self.expr(x, subs));
                    }
                    s + ")"
                }
                ExprKind::InList {
                    expr,
                    list,
                    negated,
                    ..
                } => format!(
                    "({} {}IN ({}))",
                    self.expr(expr, subs),
                    if *negated { "NOT " } else { "" },
                    self.args(list, subs)
                ),
                ExprKind::SessionValue(k) => format!("{k:?}").to_lowercase(),
                ExprKind::Aggregate(a) => self.agg(a, subs),
                ExprKind::SubLink { kind, test, query } => {
                    subs.push(query);
                    match test {
                        Some(t) => format!("{{SubLink {kind:?} test={}}}", self.expr(t, subs)),
                        None => format!("{{SubLink {kind:?}}}"),
                    }
                }
                ExprKind::SubLinkOutput(i) => format!("out{i}"),
            }
        }

        fn kind(k: JoinKind) -> &'static str {
            match k {
                JoinKind::Inner => "Inner",
                JoinKind::Left => "Left",
                JoinKind::Full => "Full",
                JoinKind::Semi => "Semi",
                JoinKind::Anti => "Anti",
            }
        }

        #[allow(clippy::too_many_lines)]
        fn head(&self, p: &'a LogicalPlan, subs: &mut Vec<&'a LogicalSubquery>) -> String {
            use LogicalPlan as L;
            match p {
                L::Get {
                    table,
                    alias,
                    cols,
                    system_columns,
                    ..
                } => {
                    let all: Vec<ColId> = cols
                        .iter()
                        .copied()
                        .chain(system_columns.iter().map(|(_, c)| *c))
                        .collect();
                    match alias {
                        Some(a) => format!("Get {} as {a} {}", table.name, self.cols(&all)),
                        None => format!("Get {} {}", table.name, self.cols(&all)),
                    }
                }
                L::Values { rows, cols } => {
                    let rows: Vec<String> = rows
                        .iter()
                        .map(|r| format!("({})", self.args(r, subs)))
                        .collect();
                    format!(
                        "Values rows={} {} {}",
                        rows.len(),
                        self.cols(cols),
                        rows.join(", ")
                    )
                }
                L::FunctionScan {
                    func,
                    args,
                    alias,
                    cols,
                } => format!(
                    "FunctionScan {}({}){} {}",
                    func.name,
                    self.args(args, subs),
                    alias.as_ref().map_or(String::new(), |a| format!(" as {a}")),
                    self.cols(cols)
                ),
                L::CteScan { cte, alias, cols } => {
                    let name = self
                        .ctes
                        .get(usize::from(cte.0))
                        .map_or("?", |c| c.name.as_str());
                    match alias {
                        Some(a) => format!("CteScan {name} as {a} {}", self.cols(cols)),
                        None => format!("CteScan {name} {}", self.cols(cols)),
                    }
                }
                L::Filter { predicate, .. } => format!("Filter {}", self.expr(predicate, subs)),
                L::Project { exprs, .. } => format!("Project {}", self.defs(exprs, subs)),
                L::Join { kind, on, .. } => match on {
                    Some(on) => format!("Join {} on {}", Self::kind(*kind), self.expr(on, subs)),
                    None => format!("Join {}", Self::kind(*kind)),
                },
                L::Aggregate { group_by, aggs, .. } => {
                    let g = self.defs(group_by, subs);
                    let a: Vec<String> = aggs
                        .iter()
                        .map(|(id, a)| format!("#{} := {}", id.0, self.agg(a, subs)))
                        .collect();
                    format!("Aggregate group={g} aggs=[{}]", a.join(" "))
                }
                L::Distinct { on, .. } => match on {
                    None => "Distinct".to_owned(),
                    Some(on) => format!("Distinct on=[{}]", self.args(on, subs)),
                },
                L::Sort { keys, .. } => {
                    let k: Vec<String> = keys
                        .iter()
                        .map(|k| {
                            format!(
                                "{} {} NULLS {}",
                                self.expr(&k.expr, subs),
                                if k.descending { "DESC" } else { "ASC" },
                                if k.nulls_first { "FIRST" } else { "LAST" }
                            )
                        })
                        .collect();
                    format!("Sort [{}]", k.join(", "))
                }
                L::Limit { limit, offset, .. } => {
                    let mut s = "Limit".to_owned();
                    if let Some(l) = limit {
                        let _ = write!(s, " limit={}", self.expr(l, subs));
                    }
                    if let Some(o) = offset {
                        let _ = write!(s, " offset={}", self.expr(o, subs));
                    }
                    s
                }
                L::SetOp {
                    op,
                    all,
                    cols,
                    left_cols,
                    right_cols,
                    ..
                } => format!(
                    "SetOp {op:?} all={all} cols={} left={} right={}",
                    self.cols(cols),
                    self.cols(left_cols),
                    self.cols(right_cols)
                ),
                L::Result {
                    one_time_filter,
                    cols,
                } => {
                    let mut s = "Result".to_owned();
                    if let Some(f) = one_time_filter {
                        let _ = write!(s, " one_time_filter={}", self.expr(f, subs));
                    }
                    if !cols.is_empty() {
                        let _ = write!(s, " {}", self.cols(cols));
                    }
                    s
                }
                L::Empty { .. } => "Empty".to_owned(),
                L::Insert {
                    table,
                    input_cols,
                    column_map,
                    ..
                } => {
                    let m: Vec<String> = column_map
                        .iter()
                        .map(|c| c.map_or("-".to_owned(), |i| i.to_string()))
                        .collect();
                    format!(
                        "Insert {} input={} column_map=[{}]",
                        table.name,
                        self.cols(input_cols),
                        m.join(" ")
                    )
                }
                L::Update {
                    table,
                    old_cols,
                    ctid,
                    new_values,
                    ..
                } => {
                    let n: Vec<String> = new_values
                        .iter()
                        .map(|(a, c)| format!("{a}:={}", self.col(*c)))
                        .collect();
                    format!(
                        "Update {} old={} ctid={} new=[{}]",
                        table.name,
                        self.cols(old_cols),
                        self.col(*ctid),
                        n.join(" ")
                    )
                }
                L::Delete {
                    table,
                    old_cols,
                    ctid,
                    ..
                } => format!(
                    "Delete {} old={} ctid={}",
                    table.name,
                    self.cols(old_cols),
                    self.col(*ctid)
                ),
            }
        }

        fn plan(&self, p: &'a LogicalPlan, depth: usize, out: &mut String) {
            let mut subs = Vec::new();
            let head = self.head(p, &mut subs);
            let _ = writeln!(out, "{}{head}", "  ".repeat(depth));
            for s in subs {
                let mut inner = String::new();
                self.plan(&s.plan, 0, &mut inner);
                for (i, line) in inner.lines().enumerate() {
                    let _ = writeln!(
                        out,
                        "{}{}{line}",
                        "  ".repeat(depth + 1),
                        if i == 0 { "~ " } else { "  " }
                    );
                }
            }
            for c in p.children() {
                self.plan(c, depth + 1, out);
            }
        }
    }
}

// ===== region: 物理（L2） =====

/// 物理プランを 1 ノード 1 行で印字する（`plan_golden` の物理側）。子は 2 スペース下げる。
/// 式は `@i`（`Local`）・`$i`（`Param`）で書く。副問い合わせは根の後に `SubPlan <id> ...` で続ける。
pub fn print_physical(q: &PhysicalQuery) -> String {
    physical_print::print(q)
}

mod physical_print {
    use std::fmt::Write as _;

    use crate::catalog::SystemColumn;
    use crate::expr::{ExprKind, PhysCol};
    use crate::planner::logical::JoinKind;
    use crate::planner::physical::{
        IndexScanKey, PhysAgg, PhysExpr, PhysicalPlan, PhysicalQuery, SubPlanStrategy,
    };
    use crate::types::{Datum, format_type, io};

    pub(super) fn print(q: &PhysicalQuery) -> String {
        let mut out = String::new();
        plan(&q.root, 0, &mut out);
        for (i, s) in q.subplans.iter().enumerate() {
            let strat = match &s.strategy {
                SubPlanStrategy::InitOnce => "init".to_owned(),
                SubPlanStrategy::Rescan => "rescan".to_owned(),
                SubPlanStrategy::Hashed {
                    probe_keys,
                    build_keys,
                } => format!(
                    "hashed probe={} build={}",
                    list(probe_keys),
                    list(build_keys)
                ),
            };
            let params: Vec<String> = s
                .params
                .iter()
                .map(|(p, e)| format!("${} := {}", p.0, expr(e)))
                .collect();
            let _ = writeln!(
                out,
                "SubPlan {i} {:?} {strat} params=[{}]{}",
                s.kind,
                params.join(", "),
                s.test
                    .as_ref()
                    .map_or(String::new(), |t| format!(" test={}", expr(t)))
            );
            plan(&s.plan, 1, &mut out);
        }
        for (i, c) in q.ctes.iter().enumerate() {
            let _ = writeln!(out, "CTE {i}");
            plan(c, 1, &mut out);
        }
        out
    }

    fn list(es: &[PhysExpr]) -> String {
        es.iter().map(expr).collect::<Vec<_>>().join(", ")
    }

    fn opt(e: Option<&PhysExpr>) -> String {
        e.map_or_else(|| "-".to_owned(), expr)
    }

    fn kind(k: JoinKind) -> &'static str {
        match k {
            JoinKind::Inner => "Inner",
            JoinKind::Left => "Left",
            JoinKind::Full => "Full",
            JoinKind::Semi => "Semi",
            JoinKind::Anti => "Anti",
        }
    }

    pub(super) fn expr(e: &PhysExpr) -> String {
        match &e.kind {
            ExprKind::Literal(d) => match d {
                Datum::Null => "NULL".to_owned(),
                Datum::Bool(b) => b.to_string(),
                Datum::Text(s) | Datum::BpChar(s) => format!("'{s}'"),
                other => io::output_text(other, e.ty).unwrap_or_else(|| format!("{other:?}")),
            },
            ExprKind::Column(PhysCol::Local(i)) => format!("@{i}"),
            ExprKind::Column(PhysCol::Param(p)) => format!("${}", p.0),
            ExprKind::Operator { op, args } => match args.as_slice() {
                [a, b] => format!("({} {} {})", expr(a), op.name, expr(b)),
                [a] => format!("({} {})", op.name, expr(a)),
                _ => format!("{}({})", op.name, list(args)),
            },
            ExprKind::Function { func, args } => format!("{}({})", func.name, list(args)),
            ExprKind::And(a) => format!("({})", join(a, " AND ")),
            ExprKind::Or(a) => format!("({})", join(a, " OR ")),
            ExprKind::Not(a) => format!("(NOT {})", expr(a)),
            ExprKind::IsNull(a) => format!("({} IS NULL)", expr(a)),
            ExprKind::IsNotNull(a) => format!("({} IS NOT NULL)", expr(a)),
            ExprKind::Cast { expr: x, .. } => format!("{}::{}", expr(x), format_type(e.ty)),
            ExprKind::SubLink { kind, query, .. } => format!("{{{kind:?} SubPlan {}}}", query.0),
            ExprKind::SubLinkOutput(i) => format!("out{i}"),
            other => {
                let s = format!("{other:?}");
                let name = s
                    .split(|c: char| !c.is_alphanumeric())
                    .next()
                    .unwrap_or("?");
                format!("<{name}>")
            }
        }
    }

    fn join(es: &[PhysExpr], sep: &str) -> String {
        es.iter().map(expr).collect::<Vec<_>>().join(sep)
    }

    fn aggs(a: &[PhysAgg]) -> String {
        a.iter()
            .map(|a| {
                format!(
                    "{:?}({}{}{}){}",
                    a.kind,
                    if a.distinct { "DISTINCT " } else { "" },
                    list(&a.args),
                    if a.order_by.is_empty() {
                        String::new()
                    } else {
                        format!(
                            " ORDER BY {}",
                            a.order_by
                                .iter()
                                .map(|k| expr(&k.expr))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    },
                    a.filter
                        .as_ref()
                        .map_or(String::new(), |f| format!(" FILTER {}", expr(f)))
                )
            })
            .collect::<Vec<_>>()
            .join(", ")
    }

    fn sys(s: &[SystemColumn]) -> String {
        if s.is_empty() {
            String::new()
        } else {
            format!(" sys={s:?}")
        }
    }

    #[allow(clippy::too_many_lines)]
    fn plan(p: &PhysicalPlan, depth: usize, out: &mut String) {
        use PhysicalPlan as P;
        let line = match p {
            P::Result {
                exprs,
                one_time_filter,
            } => format!(
                "Result exprs=[{}] otf={}",
                list(exprs),
                opt(one_time_filter.as_ref())
            ),
            P::Values { rows } => format!(
                "Values [{}]",
                rows.iter()
                    .map(|r| format!("({})", list(r)))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            P::SeqScan {
                rel,
                columns,
                system_columns,
                filter,
            } => format!(
                "SeqScan {} cols={}{} filter={}",
                rel.oid,
                columns.len(),
                sys(system_columns),
                opt(filter.as_ref())
            ),
            P::IndexScan {
                index,
                keys,
                direction,
                columns,
                system_columns,
                filter,
                ..
            } => {
                let mut k: Vec<String> = keys
                    .eq
                    .iter()
                    .map(|k| match k {
                        IndexScanKey::Eq(e) => format!("= {}", expr(e)),
                        IndexScanKey::IsNull => "IS NULL".to_owned(),
                    })
                    .collect();
                if let Some(l) = &keys.lower {
                    k.push(format!(
                        "{} {}",
                        if l.inclusive { ">=" } else { ">" },
                        expr(&l.expr)
                    ));
                }
                if let Some(u) = &keys.upper {
                    k.push(format!(
                        "{} {}",
                        if u.inclusive { "<=" } else { "<" },
                        expr(&u.expr)
                    ));
                }
                format!(
                    "IndexScan {} {direction:?} cols={}{} keys=[{}] filter={}",
                    index.name,
                    columns.len(),
                    sys(system_columns),
                    k.join(", "),
                    opt(filter.as_ref())
                )
            }
            P::FunctionScan { func, args } => format!("FunctionScan {}({})", func.name, list(args)),
            P::Filter { predicate, .. } => format!("Filter {}", expr(predicate)),
            P::Project { exprs, .. } => format!("Project [{}]", list(exprs)),
            P::Sort { keys, .. } => format!(
                "Sort [{}]",
                keys.iter()
                    .map(|k| format!(
                        "{}{}{}",
                        expr(&k.expr),
                        if k.descending { " DESC" } else { "" },
                        if k.nulls_first { " NULLS FIRST" } else { "" }
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            P::Unique { key_cols, .. } => format!("Unique {key_cols:?}"),
            P::Distinct { .. } => "Distinct".to_owned(),
            P::Limit { limit, offset, .. } => {
                format!(
                    "Limit limit={} offset={}",
                    opt(limit.as_ref()),
                    opt(offset.as_ref())
                )
            }
            P::Materialize { .. } => "Materialize".to_owned(),
            P::NestedLoopJoin {
                kind: k,
                join_filter,
                ..
            } => format!(
                "NestedLoop {} filter={}",
                kind(*k),
                opt(join_filter.as_ref())
            ),
            P::NestedLoopParam {
                kind: k,
                params,
                join_filter,
                ..
            } => format!(
                "NestedLoopParam {} params=[{}] filter={}",
                kind(*k),
                params
                    .iter()
                    .map(|(p, e)| format!("${} := {}", p.0, expr(e)))
                    .collect::<Vec<_>>()
                    .join(", "),
                opt(join_filter.as_ref())
            ),
            P::HashJoin {
                kind: k,
                left_keys,
                right_keys,
                residual,
                build_is_left,
                ..
            } => format!(
                "HashJoin {} keys=[{}]=[{}] residual={} build={}",
                kind(*k),
                list(left_keys),
                list(right_keys),
                opt(residual.as_ref()),
                if *build_is_left { "left" } else { "right" }
            ),
            P::Aggregate { aggs: a, .. } => format!("Aggregate [{}]", aggs(a)),
            P::HashAggregate { keys, aggs: a, .. } => {
                format!("HashAggregate keys=[{}] aggs=[{}]", list(keys), aggs(a))
            }
            P::GroupAggregate { keys, aggs: a, .. } => {
                format!("GroupAggregate keys=[{}] aggs=[{}]", list(keys), aggs(a))
            }
            P::Append { .. } => "Append".to_owned(),
            P::HashSetOp { op, all, .. } => format!("HashSetOp {op:?} all={all}"),
            P::CteScan { cte } => format!("CteScan {cte}"),
            P::Insert { table_name, .. } => format!("Insert {table_name}"),
            P::Update {
                table_name,
                assigned,
                ..
            } => format!("Update {table_name} assigned={assigned:?}"),
            P::Delete { n_user_cols, .. } => format!("Delete cols={n_user_cols}"),
        };
        let _ = writeln!(out, "{}{line}", "  ".repeat(depth));
        for c in p.children() {
            plan(c, depth + 1, out);
        }
    }
}
