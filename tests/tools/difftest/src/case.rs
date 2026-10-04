//! A self-contained test case (schema + rows + one action), running it on
//! both servers, and shrinking it.

use std::collections::HashSet;
use std::fmt::Write as _;

use crate::ast::{Expr, Select, UnOp};
use crate::db::{CmpOpts, Outcome, Server, compare, tlp_violation};
use crate::schema::{Insert, Lit, LitForm, Schema, Val};

#[derive(Clone, Debug)]
pub(crate) enum Action {
    None,
    Query(Select),
    Tlp { base: Select, pred: Expr },
}

#[derive(Clone, Debug)]
pub(crate) struct Case {
    pub(crate) schema: Schema,
    pub(crate) inserts: Vec<Insert>,
    pub(crate) action: Action,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum FailKind {
    /// CREATE TABLE / INSERT behaved differently.
    SetupDiff,
    /// A SELECT behaved differently.
    QueryDiff,
    /// The TLP identity does not hold on the test server.
    Tlp,
    /// The TLP identity does not hold on the reference server (generator bug).
    TlpRef,
}

impl FailKind {
    pub(crate) fn label(self) -> &'static str {
        match self {
            FailKind::SetupDiff => "setup diff",
            FailKind::QueryDiff => "query diff",
            FailKind::Tlp => "TLP violation (test server)",
            FailKind::TlpRef => "TLP violation (reference server)",
        }
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Failure {
    pub(crate) kind: FailKind,
    pub(crate) reason: String,
    pub(crate) sql: String,
    pub(crate) ref_out: Outcome,
    pub(crate) test_out: Outcome,
}

#[derive(Debug)]
pub(crate) struct Pair {
    pub(crate) r: Server,
    pub(crate) t: Server,
    pub(crate) cmp: CmpOpts,
    pub(crate) executed: u64,
}

impl Pair {
    pub(crate) fn both(&mut self, sql: &str) -> (Outcome, Outcome) {
        self.executed += 1;
        (self.r.exec(sql), self.t.exec(sql))
    }

    /// Runs `sql` on both servers and compares.
    pub(crate) fn check(
        &mut self,
        kind: FailKind,
        sql: &str,
        ordered: bool,
    ) -> Result<(Outcome, Outcome), Box<Failure>> {
        let (r, t) = self.both(sql);
        match compare(&r, &t, ordered, self.cmp) {
            None => Ok((r, t)),
            Some(reason) => Err(Box::new(Failure {
                kind,
                reason,
                sql: sql.to_string(),
                ref_out: r,
                test_out: t,
            })),
        }
    }

    pub(crate) fn drop_tables(&mut self, names: &[String]) {
        if !names.is_empty() {
            let sql = format!("DROP TABLE IF EXISTS {}", names.join(", "));
            let _ = self.r.exec(&sql);
            let _ = self.t.exec(&sql);
        }
    }
}

/// The three TLP partition queries plus the base query, in that order:
/// `[base, p, NOT p, p IS NULL]`.
pub(crate) fn tlp_queries(base: &Select, pred: &Expr) -> [Select; 4] {
    let base = base.unordered();
    let with = |p: Expr| {
        let mut q = base.clone();
        q.where_ = Some(match &base.where_ {
            Some(w) => Expr::bin(crate::ast::BinOp::And, w.clone(), p),
            None => p,
        });
        q
    };
    let p = pred.clone();
    [
        base.clone(),
        with(p.clone()),
        with(Expr::Unary {
            op: UnOp::Not,
            e: Box::new(p.clone()),
        }),
        with(Expr::IsNull {
            e: Box::new(p),
            neg: false,
        }),
    ]
}

/// Runs a TLP check (differentially and against the identity).
pub(crate) fn run_tlp(
    pair: &mut Pair,
    base: &Select,
    pred: &Expr,
    names: &[String],
) -> Option<Box<Failure>> {
    let qs = tlp_queries(base, pred);
    let mut ro = Vec::new();
    let mut to = Vec::new();
    let mut sqls = Vec::new();
    for q in &qs {
        let sql = q.render(names);
        match pair.check(FailKind::QueryDiff, &sql, false) {
            Ok((r, t)) => {
                ro.push(r);
                to.push(t);
            }
            Err(f) => return Some(f),
        }
        sqls.push(sql);
    }
    let distinct = base.distinct;
    let script = sqls.join(";\n");
    if let Some(reason) = tlp_violation(&to[0], &to[1..], distinct) {
        return Some(Box::new(Failure {
            kind: FailKind::Tlp,
            reason,
            sql: script,
            ref_out: ro[0].clone(),
            test_out: to[0].clone(),
        }));
    }
    if let Some(reason) = tlp_violation(&ro[0], &ro[1..], distinct) {
        return Some(Box::new(Failure {
            kind: FailKind::TlpRef,
            reason,
            sql: script,
            ref_out: ro[0].clone(),
            test_out: to[0].clone(),
        }));
    }
    None
}

impl Failure {
    /// Replaces the run-specific table names with `t0`, `t1`, ...
    pub(crate) fn canonicalize(&mut self, names: &[String]) {
        let mut order: Vec<usize> = (0..names.len()).collect();
        order.sort_by_key(|&i| std::cmp::Reverse(names[i].len()));
        let fix = |s: &str| {
            let mut s = s.to_string();
            for &i in &order {
                s = s.replace(&names[i], &format!("t{i}"));
            }
            s
        };
        self.sql = fix(&self.sql);
        for o in [&mut self.ref_out, &mut self.test_out] {
            if let Outcome::Error { message, .. } | Outcome::Lost(message) = o {
                *message = fix(message);
            }
        }
    }
}

impl Case {
    pub(crate) fn setup_sql(&self, names: &[String]) -> Vec<String> {
        let mut v: Vec<String> = self
            .schema
            .tables
            .iter()
            .enumerate()
            .map(|(i, t)| t.create_sql(&names[i], names))
            .collect();
        v.extend(self.inserts.iter().map(|i| i.sql(names)));
        v
    }

    pub(crate) fn action_sql(&self, names: &[String]) -> Vec<String> {
        match &self.action {
            Action::None => Vec::new(),
            Action::Query(q) => vec![q.render(names)],
            Action::Tlp { base, pred } => tlp_queries(base, pred)
                .iter()
                .map(|q| q.render(names))
                .collect(),
        }
    }

    /// Runs the whole case on fresh tables named `names`; returns the first failure.
    pub(crate) fn run(&self, pair: &mut Pair, names: &[String]) -> Option<Box<Failure>> {
        pair.drop_tables(names);
        let result = self.run_inner(pair, names);
        pair.drop_tables(names);
        result
    }

    fn run_inner(&self, pair: &mut Pair, names: &[String]) -> Option<Box<Failure>> {
        for sql in self.setup_sql(names) {
            if let Err(f) = pair.check(FailKind::SetupDiff, &sql, false) {
                return Some(f);
            }
        }
        match &self.action {
            Action::None => None,
            Action::Query(q) => pair
                .check(FailKind::QueryDiff, &q.render(names), q.ordered())
                .err(),
            Action::Tlp { base, pred } => run_tlp(pair, base, pred, names),
        }
    }

    /// Columns referenced by the action (`(table, column)`), and tables used at all.
    fn referenced(&self) -> (HashSet<(usize, usize)>, HashSet<usize>) {
        let mut cols = HashSet::new();
        let mut tables = HashSet::new();
        let mut visit = |q: &Select| {
            q.visit_cols(&mut |e| {
                if let Expr::Col { table, col, .. } = e {
                    cols.insert((*table, *col));
                }
            });
            let mut q2 = q.clone();
            q2.visit_tables_mut(&mut |t| {
                tables.insert(*t);
            });
            let mut stars = Vec::new();
            q.star_tables(&mut stars);
            stars
        };
        let mut stars = Vec::new();
        match &self.action {
            Action::None => {}
            Action::Query(q) => stars = visit(q),
            Action::Tlp { base, pred } => {
                for q in tlp_queries(base, pred) {
                    stars.extend(visit(&q));
                }
            }
        }
        for t in stars {
            for c in 0..self.schema.tables[t].cols.len() {
                cols.insert((t, c));
            }
        }
        (cols, tables)
    }

    fn map_action(&mut self, f: &mut dyn FnMut(&mut Select)) {
        match &mut self.action {
            Action::None => {}
            Action::Query(q) => f(q),
            Action::Tlp { base, pred } => {
                f(base);
                // The predicate lives in the base query's scope.
                let mut holder = base.clone();
                holder.items.clear();
                holder.where_ = Some(pred.clone());
                holder.group_by.clear();
                holder.having = None;
                for fi in &mut holder.from {
                    fi.join = None;
                }
                f(&mut holder);
                if let Some(p) = holder.where_ {
                    *pred = p;
                }
            }
        }
    }

    fn without_table(&self, t: usize) -> Case {
        let mut c = self.clone();
        c.schema.tables.remove(t);
        c.inserts.retain(|i| i.table != t);
        for i in &mut c.inserts {
            if i.table > t {
                i.table -= 1;
            }
        }
        c.map_action(&mut |q| {
            q.visit_tables_mut(&mut |x| {
                if *x > t {
                    *x -= 1;
                }
            });
            q.visit_cols_mut(&mut |e| {
                if let Expr::Col { table, .. } = e {
                    if *table > t {
                        *table -= 1;
                    }
                }
            });
        });
        c
    }

    fn without_column(&self, t: usize, col: usize) -> Case {
        let mut c = self.clone();
        let table = &mut c.schema.tables[t];
        table.cols.remove(col);
        table.checks.retain(|ck| {
            let mut uses = ck.column == Some(col);
            ck.expr.visit_cols(&mut |e| {
                if let Expr::Col { col: cc, .. } = e {
                    uses |= *cc == col;
                }
            });
            !uses
        });
        for ck in &mut table.checks {
            if let Some(cc) = &mut ck.column {
                if *cc > col {
                    *cc -= 1;
                }
            }
            ck.expr.visit_cols_mut(&mut |e| {
                if let Expr::Col { col: cc, .. } = e {
                    if *cc > col {
                        *cc -= 1;
                    }
                }
            });
        }
        for ins in &mut c.inserts {
            if ins.table != t {
                continue;
            }
            let pos = match &mut ins.cols {
                None => Some(col),
                Some(list) => {
                    let p = list.iter().position(|&x| x == col);
                    if let Some(p) = p {
                        list.remove(p);
                    }
                    for x in list.iter_mut() {
                        if *x > col {
                            *x -= 1;
                        }
                    }
                    if list.is_empty() {
                        ins.rows = vec![Vec::new()];
                    }
                    p
                }
            };
            if let Some(p) = pos {
                for row in &mut ins.rows {
                    if p < row.len() {
                        row.remove(p);
                    }
                }
            }
        }
        c.map_action(&mut |q| {
            q.visit_cols_mut(&mut |e| {
                if let Expr::Col { table, col: cc, .. } = e {
                    if *table == t && *cc > col {
                        *cc -= 1;
                    }
                }
            });
        });
        c
    }

    /// Candidate reductions, roughly from largest to smallest.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn candidates(&self) -> Vec<Case> {
        let mut out = Vec::new();
        // 1. Remove chunks of INSERT statements.
        let n = self.inserts.len();
        let mut size = n.div_ceil(2);
        while size >= 1 {
            let mut start = 0;
            while start < n {
                let mut c = self.clone();
                c.inserts.drain(start..(start + size).min(n));
                out.push(c);
                start += size;
            }
            if size == 1 {
                break;
            }
            size = size.div_ceil(2);
        }
        // 2. Remove rows of multi-row INSERTs.
        for (i, ins) in self.inserts.iter().enumerate() {
            if ins.rows.len() > 1 {
                for r in 0..ins.rows.len() {
                    let mut c = self.clone();
                    c.inserts[i].rows.remove(r);
                    out.push(c);
                }
            }
        }
        // 3. Shrink the action.
        match &self.action {
            Action::None => {}
            Action::Query(q) => {
                for s in q.shrinks(&self.schema) {
                    let mut c = self.clone();
                    c.action = Action::Query(s);
                    out.push(c);
                }
            }
            Action::Tlp { base, pred } => {
                for s in base.shrinks(&self.schema) {
                    let mut c = self.clone();
                    c.action = Action::Tlp {
                        base: s,
                        pred: pred.clone(),
                    };
                    out.push(c);
                }
                for p in pred.shrinks(&self.schema) {
                    let mut c = self.clone();
                    c.action = Action::Tlp {
                        base: base.clone(),
                        pred: p,
                    };
                    out.push(c);
                }
            }
        }
        // 4. Remove unused tables and columns.
        let (cols, tables) = self.referenced();
        for t in (0..self.schema.tables.len()).rev() {
            if !tables.contains(&t) && self.schema.tables.len() > 1 {
                out.push(self.without_table(t));
            }
        }
        for (t, table) in self.schema.tables.iter().enumerate() {
            if table.cols.len() > 1 {
                for col in (0..table.cols.len()).rev() {
                    if !cols.contains(&(t, col)) {
                        out.push(self.without_column(t, col));
                    }
                }
            }
        }
        // 5. Drop constraints.
        for (t, table) in self.schema.tables.iter().enumerate() {
            for k in 0..table.checks.len() {
                let mut c = self.clone();
                c.schema.tables[t].checks.remove(k);
                out.push(c);
            }
            for (i, col) in table.cols.iter().enumerate() {
                if col.not_null {
                    let mut c = self.clone();
                    c.schema.tables[t].cols[i].not_null = false;
                    out.push(c);
                }
                if col.default.is_some() {
                    let mut c = self.clone();
                    c.schema.tables[t].cols[i].default = None;
                    out.push(c);
                }
            }
        }
        // 6. Replace inserted values with NULL.
        for (i, ins) in self.inserts.iter().enumerate() {
            for (r, row) in ins.rows.iter().enumerate() {
                for (v, val) in row.iter().enumerate() {
                    if !matches!(val, Val::Lit(Lit::Null, LitForm::Bare, _)) {
                        let mut c = self.clone();
                        let ty = match val {
                            Val::Lit(_, _, ty) => *ty,
                            Val::Default => crate::schema::Ty::Text,
                        };
                        c.inserts[i].rows[r][v] = Val::Lit(Lit::Null, LitForm::Bare, ty);
                        out.push(c);
                    }
                }
            }
        }
        out
    }

    /// A standalone SQL script (canonical table names `t0`, `t1`, ...).
    pub(crate) fn script(&self, header: &str, failure: &Failure, max_rows: usize) -> String {
        let names: Vec<String> = (0..self.schema.tables.len())
            .map(|i| format!("t{i}"))
            .collect();
        let mut s = String::new();
        for line in header.lines() {
            let _ = writeln!(s, "-- {line}");
        }
        let _ = writeln!(s, "-- kind: {}", failure.kind.label());
        let _ = writeln!(s, "-- reason: {}", failure.reason);
        if !names.is_empty() {
            let _ = writeln!(s, "DROP TABLE IF EXISTS {};", names.join(", "));
        }
        for sql in self.setup_sql(&names) {
            let _ = writeln!(s, "{sql};");
        }
        if let Action::Tlp { .. } = self.action {
            let _ = writeln!(
                s,
                "-- TLP: rows(Q1) must equal rows(Q2) + rows(Q3) + rows(Q4) (as bags{})",
                if matches!(&self.action, Action::Tlp { base, .. } if base.distinct) {
                    ", duplicates removed"
                } else {
                    ""
                }
            );
        }
        for sql in self.action_sql(&names) {
            let _ = writeln!(s, "{sql};");
        }
        let _ = writeln!(s, "-- failing statement:");
        for line in failure.sql.lines() {
            let _ = writeln!(s, "--   {line}");
        }
        for (label, o) in [("reference", &failure.ref_out), ("test", &failure.test_out)] {
            let text = o.summary(max_rows);
            let mut lines = text.lines();
            let _ = writeln!(s, "-- {label:9}: {}", lines.next().unwrap_or(""));
            for l in lines {
                let _ = writeln!(s, "--          {l}");
            }
        }
        if !names.is_empty() {
            let _ = writeln!(s, "DROP TABLE IF EXISTS {};", names.join(", "));
        }
        s
    }
}
