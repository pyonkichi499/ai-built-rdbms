//! slt の lint（11-tests-plan.md §3.2.5 の L01〜L10）。L11（plan_variants の生成物）は未実装。

use crate::parse::{self, Kind, Record, SltFile};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

pub struct Diag {
    pub path: PathBuf,
    pub line: usize,
    pub rule: &'static str,
    pub msg: String,
    pub warning: bool,
}

pub struct Options {
    pub only: Vec<String>,
    pub known_diffs: Option<PathBuf>,
    /// 全体（既定のパス）を検査しているとき true。使われていない KD の検出に使う
    pub whole: bool,
}

fn enabled(o: &Options, rule: &str) -> bool {
    o.only.is_empty() || o.only.iter().any(|r| r.eq_ignore_ascii_case(rule))
}

fn path_has(p: &Path, comp: &str) -> bool {
    p.components().any(|c| c.as_os_str() == comp)
}

fn is_m4(p: &Path) -> bool {
    path_has(p, "m4")
}

fn is_restart(p: &Path) -> bool {
    path_has(p, "restart")
}

/// tests/slt/m4/<dir>/ の許可される接頭辞。tests/restart/m4 は `m4r_`。m4 の外は検査しない。
fn allowed_prefixes(p: &Path) -> Option<Vec<&'static str>> {
    if !is_m4(p) {
        return None;
    }
    if is_restart(p) {
        return Some(vec!["m4r_"]);
    }
    let comps: Vec<String> = p
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let pos = comps.iter().position(|c| c == "m4")?;
    let dir = comps.get(pos + 1)?;
    Some(match dir.as_str() {
        "join" => vec!["jn_"],
        "agg" => vec!["ag_"],
        "setop" => vec!["so_"],
        "cte" => vec!["ct_"],
        "dml" => vec!["dm_"],
        "subquery" => vec!["sb_"],
        "types" => vec!["ty_"],
        "catalog" => vec!["cat_"],
        "constraint" => vec!["cst_"],
        "index" => vec!["idx_"],
        "ddl" => vec!["ddl_"],
        "seq" => vec!["sq_", "sr_", "id_"],
        "explain" => vec!["ex_"],
        "copy" => vec!["cp_"],
        "psql" => vec!["psql_"],
        "plan_variants" => vec!["pv_"],
        "mem" => vec!["mem_"],
        _ => return None,
    })
}

#[derive(Clone)]
struct Created {
    kind: &'static str,
    name: String,
    on_table: Option<String>,
    line: usize,
}

/// `a . b` のような修飾名を読む。(最後の名前, 次の位置)
fn read_name(t: &[String], mut i: usize) -> Option<(String, usize)> {
    let mut name = t.get(i)?.clone();
    i += 1;
    while t.get(i).map(String::as_str) == Some(".") && i + 1 < t.len() {
        name = t[i + 1].clone();
        i += 2;
    }
    Some((name, i))
}

fn parse_create(st: &[String], line: usize) -> Option<Created> {
    if st.first().map(String::as_str) != Some("create") {
        return None;
    }
    let mut i = 1;
    while matches!(
        st.get(i).map(String::as_str),
        Some("unique" | "global" | "local")
    ) {
        i += 1;
    }
    if matches!(
        st.get(i).map(String::as_str),
        Some("temp" | "temporary" | "unlogged")
    ) {
        return None;
    }
    let kind = match st.get(i).map(String::as_str)? {
        "table" => "table",
        "index" => "index",
        "sequence" => "sequence",
        _ => return None,
    };
    i += 1;
    if st.get(i).map(String::as_str) == Some("concurrently") {
        i += 1;
    }
    if st.get(i).map(String::as_str) == Some("if")
        && st.get(i + 1).map(String::as_str) == Some("not")
    {
        i += 3;
    }
    if kind == "index" && st.get(i).map(String::as_str) == Some("on") {
        return None; // 名前のない CREATE INDEX ON ...
    }
    let (name, j) = read_name(st, i)?;
    let mut on_table = None;
    if kind == "index" && st.get(j).map(String::as_str) == Some("on") {
        let mut k = j + 1;
        if st.get(k).map(String::as_str) == Some("only") {
            k += 1;
        }
        on_table = read_name(st, k).map(|x| x.0);
    }
    Some(Created {
        kind,
        name,
        on_table,
        line,
    })
}

fn parse_drop(st: &[String]) -> Vec<String> {
    let mut out = Vec::new();
    if st.first().map(String::as_str) != Some("drop") {
        return out;
    }
    if !matches!(
        st.get(1).map(String::as_str),
        Some("table" | "index" | "sequence")
    ) {
        return out;
    }
    let mut i = 2;
    if st.get(i).map(String::as_str) == Some("concurrently") {
        i += 1;
    }
    if st.get(i).map(String::as_str) == Some("if") {
        i += 2;
    }
    while let Some((n, j)) = read_name(st, i) {
        out.push(n);
        if st.get(j).map(String::as_str) == Some(",") {
            i = j + 1;
        } else {
            break;
        }
    }
    out
}

#[derive(Default)]
struct FileFacts {
    created: Vec<Created>,
    dropped: BTreeSet<String>,
}

/// トランザクションのブロックを追いながら、作ったオブジェクトと DROP したオブジェクトを集める。
fn collect_facts(f: &SltFile) -> FileFacts {
    let mut facts = FileFacts::default();
    let mut in_txn = false;
    let mut pending: Vec<Created> = Vec::new();
    for r in &f.records {
        let failing = matches!(
            r.kind,
            Kind::Statement { error: true, .. } | Kind::Query { error: true, .. }
        );
        let toks = parse::tokens(&r.sql);
        for st in parse::split_statements(&toks) {
            let w0 = st[0].as_str();
            match w0 {
                "begin" | "start" => in_txn = true,
                "commit" | "end" => {
                    in_txn = false;
                    facts.created.append(&mut pending);
                }
                "rollback" | "abort" if !st.iter().any(|t| t == "to") => {
                    in_txn = false;
                    pending.clear();
                }
                _ => {}
            }
            if failing {
                continue;
            }
            if let Some(c) = parse_create(&st, r.sql_line) {
                if in_txn {
                    pending.push(c);
                } else {
                    facts.created.push(c);
                }
            }
            for n in parse_drop(&st) {
                facts.dropped.insert(n);
            }
        }
    }
    facts.created.append(&mut pending);
    facts
}

fn top_level_has(toks: &[String], word: &str) -> bool {
    let mut depth = 0i32;
    for t in toks {
        match t.as_str() {
            "(" => depth += 1,
            ")" => depth -= 1,
            x if depth == 0 && x == word => return true,
            _ => {}
        }
    }
    false
}

const VOLATILE: [&str; 5] = [
    "now",
    "clock_timestamp",
    "random",
    "current_timestamp",
    "statement_timestamp",
];

fn l09_hit(toks: &[String]) -> Option<&'static str> {
    if toks.first().map(String::as_str) != Some("select") {
        return None;
    }
    let mut depth = 0i32;
    for (i, t) in toks.iter().enumerate() {
        match t.as_str() {
            "(" => depth += 1,
            ")" => depth -= 1,
            "from" if depth == 0 => break,
            _ => {}
        }
        let Some(v) = VOLATILE.iter().find(|v| **v == t) else {
            continue;
        };
        if depth != 0 {
            continue;
        }
        let prev = if i == 0 { "" } else { toks[i - 1].as_str() };
        // 関数呼び出し `name ( )` の末尾の次を見る
        let mut j = i + 1;
        if toks.get(j).map(String::as_str) == Some("(")
            && toks.get(j + 1).map(String::as_str) == Some(")")
        {
            j += 2;
        }
        let next = toks.get(j).map_or("", String::as_str);
        let prev_ok = matches!(prev, "select" | "," | "distinct");
        let next_ok = matches!(next, "" | "," | "from" | "as" | ";");
        if prev_ok && next_ok {
            return Some(v);
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
fn lint_file(
    path: &Path,
    f: &SltFile,
    o: &Options,
    facts: &FileFacts,
    shared_dropped: &BTreeSet<String>,
    kd_used: &mut BTreeSet<String>,
    kd_defined: &BTreeSet<String>,
    out: &mut Vec<Diag>,
) {
    let mut push = |line: usize, rule: &'static str, msg: String, warning: bool| {
        if enabled(o, rule) {
            out.push(Diag {
                path: path.to_path_buf(),
                line,
                rule,
                msg,
                warning,
            });
        }
    };

    let raw_lines: Vec<String> = std::fs::read_to_string(path)
        .map(|s| s.lines().map(str::to_string).collect())
        .unwrap_or_default();
    // L01 / L03
    for c in &facts.created {
        // 直前の行が `# LINT-ALLOW: L01 ...` なら DROP を要求しない（コミットされない作成など）
        let allow_l01 = (1..=3).any(|d| {
            c.line
                .checked_sub(d + 1)
                .and_then(|i| raw_lines.get(i))
                .is_some_and(|l| l.trim_start().starts_with("# LINT-ALLOW: L01"))
        });
        let dropped = shared_dropped.contains(&c.name)
            || allow_l01
            || c.on_table
                .as_ref()
                .is_some_and(|t| shared_dropped.contains(t));
        if !dropped {
            push(
                c.line,
                "L01",
                format!("{} {} が DROP されていない", c.kind, c.name),
                false,
            );
        }
        if let Some(prefixes) = allowed_prefixes(path)
            && !prefixes.iter().any(|p| c.name.starts_with(p))
        {
            push(
                c.line,
                "L03",
                format!(
                    "{} {} が接頭辞 {} で始まらない",
                    c.kind,
                    c.name,
                    prefixes.join(" / ")
                ),
                false,
            );
        }
    }

    // L02: SET に対応する RESET
    let mut sets: Vec<(String, usize)> = Vec::new();
    let mut resets: BTreeSet<String> = BTreeSet::new();
    let mut in_txn = false;
    for r in &f.records {
        let is_error = matches!(
            r.kind,
            parse::Kind::Statement { error: true, .. } | parse::Kind::Query { error: true, .. }
        );
        for st in parse::split_statements(&parse::tokens(&r.sql)) {
            let w = |i: usize| st.get(i).map_or("", String::as_str);
            match w(0) {
                "begin" | "start" => in_txn = true,
                "commit" | "end" | "rollback" | "abort" if w(1) != "to" => in_txn = false,
                "set" if !in_txn && !is_error => {
                    let mut i = 1;
                    if w(i) == "session" && w(i + 1) != "authorization" {
                        i += 1;
                    }
                    if w(i) == "local" {
                        continue;
                    }
                    if matches!(w(i), "transaction" | "characteristics" | "constraints") {
                        continue;
                    }
                    let name = if (w(i) == "time" && w(i + 1) == "zone") || w(i) == "timezone" {
                        "time zone".to_string()
                    } else {
                        w(i).to_string()
                    };
                    let to_default = st.iter().any(|t| t == "default")
                        || (name == "time zone" && matches!(w(i + 2), "default" | "local"));
                    if to_default {
                        resets.insert(name);
                    } else if !name.is_empty() {
                        sets.push((name, r.line));
                    }
                }
                "reset" => {
                    if (w(1) == "time" && w(2) == "zone") || w(1) == "timezone" {
                        resets.insert("time zone".into());
                    } else {
                        resets.insert(w(1).to_string());
                    }
                }
                _ => {}
            }
        }
    }
    let reset_all = resets.contains("all");
    for (name, line) in sets {
        if !reset_all && !resets.contains(&name) {
            push(
                line,
                "L02",
                format!("SET {name} に対応する RESET {name} がない"),
                false,
            );
        }
    }

    for r in &f.records {
        lint_record(path, f, r, o, kd_used, kd_defined, &mut push);
    }

    // L10: 大きさ
    if f.lines.len() > 1500 {
        push(
            1,
            "L10",
            format!("{} 行（1,500 行を超える）", f.lines.len()),
            true,
        );
    }
}

fn lint_record(
    path: &Path,
    f: &SltFile,
    r: &Record,
    _o: &Options,
    kd_used: &mut BTreeSet<String>,
    kd_defined: &BTreeSet<String>,
    push: &mut impl FnMut(usize, &'static str, String, bool),
) {
    let toks = parse::tokens(&r.sql);

    // L05: エラーの形
    let is_err = matches!(
        r.kind,
        Kind::Statement { error: true, .. } | Kind::Query { error: true, .. }
    );
    if is_err {
        let after = r
            .directive
            .split_whitespace()
            .skip(2)
            .collect::<Vec<_>>()
            .join(" ");
        if after.is_empty() && r.expected.is_empty() {
            push(
                r.line,
                "L05",
                "error に (SQLSTATE) もメッセージの正規表現もない".into(),
                false,
            );
        }
        if after.contains("db error:") || r.expected.iter().any(|e| e.contains("db error:")) {
            push(
                r.line,
                "L05",
                "`db error:` を含めない（メッセージの本体だけを照合する）".into(),
                false,
            );
        }
    }

    // L06: 既知の差
    if is_m4(path) {
        for (_only, label, line) in &r.conds {
            if label != "yuzhu" {
                continue;
            }
            let prev = if *line >= 2 {
                f.lines[*line - 2].trim()
            } else {
                ""
            };
            if let Some(rest) = prev.strip_prefix("# KNOWN-DIFF:") {
                let id = rest.split_whitespace().next().unwrap_or("");
                if !id.starts_with("KD-") {
                    push(
                        *line,
                        "L06",
                        "`# KNOWN-DIFF: KD-<n> 理由` の形にする".into(),
                        false,
                    );
                } else {
                    kd_used.insert(id.to_string());
                    if !kd_defined.is_empty() && !kd_defined.contains(id) {
                        push(
                            *line,
                            "L06",
                            format!("{id} が KNOWN-DIFFS.md にない"),
                            false,
                        );
                    }
                }
            } else if !prev.starts_with("# YUZHU-ONLY:") {
                push(*line, "L06", "skipif / onlyif yuzhu の直前の行が `# KNOWN-DIFF: KD-<n> ...` か `# YUZHU-ONLY: ...` でない".into(), false);
            }
        }
    }

    if let Kind::Statement { count, .. } = &r.kind {
        // L08
        if *count == Some(0)
            && matches!(
                toks.first().map(String::as_str),
                Some("select" | "with" | "values")
            )
        {
            push(
                r.line,
                "L08",
                "SELECT に statement count 0 を付けない（query T + 空の期待値にする）".into(),
                false,
            );
        }
    }

    if let Kind::Query {
        types,
        sort,
        error: false,
    } = &r.kind
    {
        let has_order = top_level_has(&toks, "order");
        let first = toks.first().map_or("", String::as_str);
        // L04
        if sort == "rowsort" && has_order {
            push(
                r.line,
                "L04",
                "rowsort を付けた問い合わせの最上位に ORDER BY がある".into(),
                false,
            );
        }
        let hash = r
            .expected
            .first()
            .is_some_and(|e| e.contains("values hashing to"));
        if sort.is_empty()
            && !has_order
            && r.expected.len() > 1
            && !hash
            && !matches!(first, "explain" | "show")
        {
            push(
                r.line,
                "L04",
                "複数行の結果で ORDER BY も rowsort もない".into(),
                true,
            );
        }
        // L07
        if let Some(e) = r.expected.first()
            && !hash
            && e != "(empty)"
            && !types.is_empty()
            && sort != "valuesort"
        {
            let cols = e.split_whitespace().count();
            let n = types.chars().count();
            let ok = if types.contains(['T', '?']) {
                cols >= n
            } else {
                cols == n
            };
            if !ok {
                push(
                    r.line,
                    "L07",
                    format!("型文字 {types}（{n} 列）と最初の期待行の列数 {cols} が合わない"),
                    false,
                );
            }
        }
        // L09
        if let Some(v) = l09_hit(&toks) {
            push(
                r.line,
                "L09",
                format!(
                    "{v} を SELECT 句に直接置いている（pg_typeof / IS NOT NULL / ::date で包む）"
                ),
                false,
            );
        }
    }
}

/// KNOWN-DIFFS.md から `KD-<n>` の ID を集める（表の行の先頭の列）。
pub fn load_known_diffs(p: &Path) -> BTreeSet<String> {
    let mut s = BTreeSet::new();
    let Ok(text) = std::fs::read_to_string(p) else {
        return s;
    };
    for l in text.lines() {
        if let Some(rest) = l.trim().strip_prefix("| KD-") {
            let id: String = rest.chars().take_while(char::is_ascii_digit).collect();
            if !id.is_empty() {
                s.insert(format!("KD-{id}"));
            }
        }
    }
    s
}

pub fn collect_slt(paths: &[PathBuf]) -> Vec<PathBuf> {
    fn walk(p: &Path, out: &mut Vec<PathBuf>) {
        if p.is_dir() {
            let mut es: Vec<PathBuf> = std::fs::read_dir(p)
                .map(|d| d.filter_map(Result::ok).map(|e| e.path()).collect())
                .unwrap_or_default();
            es.sort();
            for e in es {
                walk(&e, out);
            }
        } else if p.extension().is_some_and(|e| e == "slt") {
            out.push(p.to_path_buf());
        }
    }
    let mut out = Vec::new();
    for p in paths {
        walk(p, &mut out);
    }
    out
}

pub fn run(paths: &[PathBuf], o: &Options) -> Vec<Diag> {
    let files = collect_slt(paths);
    let kd_defined = o
        .known_diffs
        .as_deref()
        .map(load_known_diffs)
        .unwrap_or_default();
    let mut kd_used = BTreeSet::new();
    let mut out = Vec::new();

    let parsed: Vec<(PathBuf, SltFile)> = files
        .into_iter()
        .filter_map(|p| match std::fs::read_to_string(&p) {
            Ok(t) => Some((p.clone(), parse::parse(&t))),
            Err(e) => {
                out.push(Diag {
                    path: p,
                    line: 0,
                    rule: "IO",
                    msg: e.to_string(),
                    warning: false,
                });
                None
            }
        })
        .collect();
    let facts: Vec<FileFacts> = parsed.iter().map(|(_, f)| collect_facts(f)).collect();

    // 再起動テストのシナリオは、フェーズをまたいで DROP を数える（最後のフェーズで消せばよい）。
    let mut scenario_dropped: BTreeMap<PathBuf, BTreeSet<String>> = BTreeMap::new();
    for ((p, _), fa) in parsed.iter().zip(&facts) {
        if is_restart(p)
            && let Some(d) = p.parent()
        {
            scenario_dropped
                .entry(d.to_path_buf())
                .or_default()
                .extend(fa.dropped.iter().cloned());
        }
    }
    for ((p, f), fa) in parsed.iter().zip(&facts) {
        let shared = if is_restart(p) {
            p.parent()
                .and_then(|d| scenario_dropped.get(d))
                .cloned()
                .unwrap_or_default()
        } else {
            fa.dropped.clone()
        };
        // z_final は何も作らない前提（作っていれば L01 になる）
        lint_file(p, f, o, fa, &shared, &mut kd_used, &kd_defined, &mut out);
    }

    if o.whole && enabled(o, "L06") {
        for id in kd_defined.difference(&kd_used) {
            out.push(Diag {
                path: o.known_diffs.clone().unwrap_or_default(),
                line: 0,
                rule: "L06",
                msg: format!("{id} の使用箇所がない"),
                warning: true,
            });
        }
    }
    out
}

pub fn format_diag(d: &Diag) -> String {
    let mut s = String::new();
    let _ = write!(
        s,
        "{}:{}: {}{}: {}",
        d.path.display(),
        d.line,
        d.rule,
        if d.warning { " (warning)" } else { "" },
        d.msg
    );
    s
}
