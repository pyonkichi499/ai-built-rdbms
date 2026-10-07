//! plan-variants: tests/gen/plan_variants/<family>.tpl から tests/slt/m4/plan_variants/<family>.slt を作る
//! （11-tests-plan.md §3.3）。
//!
//!   slttools plan-variants base    tests/gen/out/<family>.base.slt（default プロファイルだけ。期待値は空）を作る
//!   slttools plan-variants expand  期待値つきの base を全プロファイルに展開する
//!   slttools plan-variants check   コミット済みの生成物を、その default ブロックから再展開して一致を確かめる
//!
//! 期待値は PostgreSQL に `sqllogictest --override` を流して base に埋める（yuzhu には使わない）。

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

const PROFILES: &[(&str, &[&str])] = &[
    ("default", &[]),
    ("no_hashjoin", &["enable_hashjoin"]),
    ("no_nestloop", &["enable_nestloop"]),
    (
        "no_hashjoin_no_nestloop",
        &["enable_hashjoin", "enable_nestloop"],
    ),
    ("no_indexscan", &["enable_indexscan"]),
    ("no_seqscan", &["enable_seqscan"]),
    ("no_hashagg", &["enable_hashagg"]),
    ("no_material", &["enable_material"]),
    ("no_sort", &["enable_sort"]),
    (
        "combined",
        &["enable_hashjoin", "enable_hashagg", "enable_indexscan"],
    ),
];

struct Query {
    /// `-- s:` の文（`statement ok`）。期待値を持たない
    stmt: bool,
    /// `query` の行の型文字列（例: `ITT`）と並べ替えの指定
    types: String,
    rowsort: bool,
    sql: Vec<String>,
}

struct Tpl {
    family: String,
    profiles: Vec<String>,
    setup: Vec<String>,
    queries: Vec<Query>,
    teardown: Vec<String>,
}

fn parse_tpl(text: &str) -> Result<Tpl, String> {
    enum Sec {
        Head,
        Setup,
        Query,
        Teardown,
    }
    let mut t = Tpl {
        family: String::new(),
        profiles: Vec::new(),
        setup: Vec::new(),
        queries: Vec::new(),
        teardown: Vec::new(),
    };
    let mut sec = Sec::Head;
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim_end();
        if let Some(rest) = line.strip_prefix("-- family:") {
            t.family = rest.split('#').next().unwrap_or("").trim().to_string();
        } else if let Some(rest) = line.strip_prefix("-- profiles:") {
            t.profiles = rest
                .split('#')
                .next()
                .unwrap_or("")
                .split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect();
        } else if line == "-- setup" {
            sec = Sec::Setup;
        } else if line == "-- teardown" {
            sec = Sec::Teardown;
        } else if line == "-- s" {
            t.queries.push(Query {
                stmt: true,
                types: String::new(),
                rowsort: false,
                sql: Vec::new(),
            });
            sec = Sec::Query;
        } else if let Some(rest) = line.strip_prefix("-- q:") {
            let mut rowsort = false;
            let mut types = String::new();
            for w in rest.split_whitespace() {
                match w {
                    "rowsort" => rowsort = true,
                    "ordered" => {}
                    w => types = w.to_string(),
                }
            }
            if types.is_empty() {
                return Err(format!("{}: 型文字列がない", n + 1));
            }
            t.queries.push(Query {
                stmt: false,
                types,
                rowsort,
                sql: Vec::new(),
            });
            sec = Sec::Query;
        } else if line.is_empty() || line.starts_with('#') {
        } else {
            match sec {
                Sec::Head => return Err(format!("{}: ヘッダの後に -- setup が必要", n + 1)),
                Sec::Setup => t.setup.push(line.to_string()),
                Sec::Teardown => t.teardown.push(line.to_string()),
                Sec::Query => t
                    .queries
                    .last_mut()
                    .expect("query")
                    .sql
                    .push(line.to_string()),
            }
        }
    }
    if t.family.is_empty() || t.profiles.is_empty() {
        return Err("family / profiles がない".into());
    }
    for p in &t.profiles {
        if !PROFILES.iter().any(|(n, _)| n == p) {
            return Err(format!("未知のプロファイル {p}"));
        }
    }
    if t.queries.iter().any(|q| q.sql.is_empty()) {
        return Err("SQL のない -- q: がある".into());
    }
    Ok(t)
}

fn query_head(q: &Query) -> String {
    if q.rowsort {
        format!("query {} rowsort", q.types)
    } else {
        format!("query {}", q.types)
    }
}

fn base_text(t: &Tpl) -> String {
    let mut s = String::new();
    let _ = writeln!(
        s,
        "# plan_variants/{}: 期待値は PostgreSQL（--override）で作る。`slttools plan-variants expand` で展開する。\n",
        t.family
    );
    for st in &t.setup {
        let _ = writeln!(s, "statement ok\n{st}\n");
    }
    for q in &t.queries {
        if q.stmt {
            let _ = writeln!(s, "statement ok\n{}\n", q.sql.join("\n"));
        } else {
            let _ = writeln!(s, "{}\n{}\n----\n", query_head(q), q.sql.join("\n"));
        }
    }
    for st in &t.teardown {
        let _ = writeln!(s, "statement ok\n{st}\n");
    }
    s
}

/// `query` レコードごとの期待値の行を順に取り出す。
fn expected_blocks(text: &str) -> Vec<Vec<String>> {
    let mut out = Vec::new();
    let mut lines = text.lines().peekable();
    let mut in_query = false;
    while let Some(l) = lines.next() {
        if l == "statement count 0" {
            // --override は行が 0 件の query をこの形に書き直す
            out.push(Vec::new());
        } else if l.starts_with("query ") {
            in_query = true;
        } else if in_query && l == "----" {
            let mut rows = Vec::new();
            while let Some(&n) = lines.peek() {
                if n.is_empty() {
                    break;
                }
                rows.push(n.to_string());
                lines.next();
            }
            out.push(rows);
            in_query = false;
        } else if l.is_empty() {
            in_query = false;
        }
    }
    out
}

fn expand_text(t: &Tpl, expected: &[Vec<String>]) -> Result<String, String> {
    let nq = t.queries.iter().filter(|q| !q.stmt).count();
    if expected.len() != nq {
        return Err(format!(
            "期待値のブロックが {} 個、問い合わせが {nq} 個",
            expected.len()
        ));
    }
    let mut s = String::new();
    let _ = writeln!(
        s,
        "# 生成物（tests/gen/plan_variants/{0}.tpl から `slttools plan-variants expand` で作る。手で直さない）。\n# 同じ問い合わせを enable_* の設定を変えて流し、結果が同じことを確かめる。\n",
        t.family
    );
    for st in &t.setup {
        let _ = writeln!(s, "statement ok\n{st}\n");
    }
    for pname in &t.profiles {
        let gucs = PROFILES
            .iter()
            .find(|(n, _)| n == pname)
            .map(|(_, g)| *g)
            .unwrap_or(&[]);
        let _ = writeln!(s, "# profile: {pname}\n");
        for g in gucs {
            let _ = writeln!(s, "statement ok\nSET {g} = off\n");
        }
        let mut exp = expected.iter();
        for q in &t.queries {
            if q.stmt {
                let _ = writeln!(s, "statement ok\n{}\n", q.sql.join("\n"));
                continue;
            }
            let _ = writeln!(s, "{}\n{}\n----", query_head(q), q.sql.join("\n"));
            for r in exp.next().into_iter().flatten() {
                let _ = writeln!(s, "{r}");
            }
            s.push('\n');
        }
        for g in gucs {
            let _ = writeln!(s, "statement ok\nRESET {g}\n");
        }
    }
    for st in &t.teardown {
        let _ = writeln!(s, "statement ok\n{st}\n");
    }
    while s.ends_with("\n\n") {
        s.pop();
    }
    Ok(s)
}

/// 生成物の default プロファイルのブロックだけを取り出して、その期待値を返す。
fn default_expected(generated: &str) -> Vec<Vec<String>> {
    let start = generated.find("# profile: default").unwrap_or(0);
    let end = generated[start + 1..]
        .find("# profile: ")
        .map_or(generated.len(), |i| start + 1 + i);
    expected_blocks(&generated[start..end])
}

fn tpl_files(root: &Path) -> Vec<PathBuf> {
    let mut v: Vec<PathBuf> = fs::read_dir(root.join("tests/gen/plan_variants"))
        .map(|d| {
            d.filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "tpl"))
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

pub fn run(root: &Path, args: &[String]) -> Result<Vec<PathBuf>, String> {
    let sub = args.first().map(String::as_str).unwrap_or("");
    let out_dir = root.join("tests/gen/out");
    let dst_dir = root.join("tests/slt/m4/plan_variants");
    let mut bad = Vec::new();
    for p in tpl_files(root) {
        let text = fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
        let t = parse_tpl(&text).map_err(|e| format!("{}: {e}", p.display()))?;
        let dst = dst_dir.join(format!("{}.slt", t.family));
        let base = out_dir.join(format!("{}.base.slt", t.family));
        match sub {
            "base" => {
                fs::create_dir_all(&out_dir).map_err(|e| e.to_string())?;
                fs::write(&base, base_text(&t)).map_err(|e| e.to_string())?;
            }
            "expand" => {
                let b = fs::read_to_string(&base).map_err(|e| format!("{}: {e}", base.display()))?;
                let s = expand_text(&t, &expected_blocks(&b))
                    .map_err(|e| format!("{}: {e}", base.display()))?;
                fs::create_dir_all(&dst_dir).map_err(|e| e.to_string())?;
                fs::write(&dst, s).map_err(|e| e.to_string())?;
            }
            "check" => {
                let cur = fs::read_to_string(&dst).unwrap_or_default();
                let s = expand_text(&t, &default_expected(&cur))
                    .map_err(|e| format!("{}: {e}", dst.display()))?;
                if s != cur {
                    bad.push(dst);
                }
            }
            _ => return Err("usage: slttools plan-variants base|expand|check".into()),
        }
    }
    Ok(bad)
}
