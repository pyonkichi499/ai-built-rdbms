//! `plan_golden`: 「SQL → build 直後 → 各ルールの後」をテキストで固定するスナップショットテスト
//! （`m4/04` §11.1）。
//!
//! `cases/*.golden` を読み、1 ケースずつ実行して比べる。`UPDATE_GOLDEN=1` で論理の段階の期待値を書き換える
//! （物理・EXPLAIN の段階（`-- physical`・`-- explain`）は別の担当のもので、このランナーは触らず、そのまま残す）。

mod fixture;

use std::fmt::Write as _;
use std::path::PathBuf;

/// ケースの区切りと、論理の段階の名前（`build` とルールの名前）。
const LOGICAL_STAGES: &[&str] = &[
    "build",
    "const_fold",
    "sublink",
    "subquery_pullup",
    "outer_join",
    "pushdown",
    "join_keys",
    "join_order",
    "prune",
    "error",
];

#[derive(Default)]
struct Case {
    name: String,
    schema: String,
    nblocks: Vec<(String, u32)>,
    settings: String,
    sql: String,
    /// 論理の段階の期待値（ファイルの順）。
    logical: Vec<(String, String)>,
    /// 論理以外のセクション（`physical` など）。書き戻しでそのまま残す。
    other: Vec<(String, String)>,
}

fn parse(text: &str) -> Vec<Case> {
    let mut cases: Vec<Case> = Vec::new();
    let mut section = String::new();
    let mut buf = String::new();
    let flush = |cases: &mut Vec<Case>, section: &str, buf: &mut String| {
        let Some(c) = cases.last_mut() else {
            buf.clear();
            return;
        };
        let body = std::mem::take(buf);
        match section {
            "schema" => c.schema = body,
            "nblocks" => {
                for l in body.lines().filter(|l| !l.trim().is_empty()) {
                    if let Some((t, n)) = l.split_once('=') {
                        c.nblocks.push((
                            t.trim().to_owned(),
                            n.trim().parse().expect("nblocks value"),
                        ));
                    }
                }
            }
            "settings" => c.settings = body,
            "sql" => c.sql = body,
            s if LOGICAL_STAGES.contains(&s) => c.logical.push((s.to_owned(), body)),
            "" => {}
            s => c.other.push((s.to_owned(), body)),
        }
    };
    for line in text.lines() {
        if let Some(name) = line.strip_prefix("-- case ") {
            flush(&mut cases, &section, &mut buf);
            cases.push(Case {
                name: name.trim().to_owned(),
                ..Case::default()
            });
            section.clear();
        } else if let Some(s) = line.strip_prefix("-- ") {
            flush(&mut cases, &section, &mut buf);
            s.trim().clone_into(&mut section);
        } else {
            buf.push_str(line);
            buf.push('\n');
        }
    }
    flush(&mut cases, &section, &mut buf);
    cases
}

/// 実行結果を期待値の形（変わらない段階は `(unchanged)`）にする。
fn actual(stages: &[(String, String)]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut prev: Option<&str> = None;
    for (name, text) in stages {
        let shown = if prev == Some(text.as_str()) {
            "(unchanged)\n".to_owned()
        } else {
            text.clone()
        };
        out.push((name.clone(), shown));
        prev = Some(text);
    }
    out
}

fn render(c: &Case, logical: &[(String, String)]) -> String {
    let mut s = String::new();
    let _ = writeln!(s, "-- case {}", c.name);
    let _ = write!(s, "-- schema\n{}", c.schema);
    if !c.nblocks.is_empty() {
        s.push_str("-- nblocks\n");
        for (t, n) in &c.nblocks {
            let _ = writeln!(s, "{t} = {n}");
        }
    }
    if !c.settings.trim().is_empty() {
        let _ = write!(s, "-- settings\n{}", c.settings);
    }
    let _ = write!(s, "-- sql\n{}", c.sql);
    for (n, b) in logical.iter().chain(&c.other) {
        let _ = write!(s, "-- {n}\n{b}");
    }
    s
}

#[allow(clippy::many_single_char_names)]
fn diff(name: &str, want: &str, got: &str) -> String {
    let mut s = format!("--- {name}: expected\n+++ {name}: actual\n");
    let (w, g): (Vec<&str>, Vec<&str>) = (want.lines().collect(), got.lines().collect());
    for i in 0..w.len().max(g.len()) {
        let (a, b) = (w.get(i), g.get(i));
        if a != b {
            if let Some(a) = a {
                let _ = writeln!(s, "-{a}");
            }
            if let Some(b) = b {
                let _ = writeln!(s, "+{b}");
            }
        } else if let Some(a) = a {
            let _ = writeln!(s, " {a}");
        }
    }
    s
}

#[test]
fn plan_golden() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/plan_golden/cases");
    let update = std::env::var_os("UPDATE_GOLDEN").is_some();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .expect("cases dir")
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "golden"))
        .collect();
    files.sort();
    let mut failures = String::new();
    let mut n_cases = 0;
    for path in files {
        let text = std::fs::read_to_string(&path).expect("read golden");
        let cases = parse(&text);
        let mut rewritten = String::new();
        for c in &cases {
            n_cases += 1;
            let got = match fixture::build(&c.schema, &c.nblocks) {
                Ok(f) => actual(&fixture::logical_stages(&f, &c.sql)),
                Err(e) => vec![("error".to_owned(), format!("{e}\n"))],
            };
            rewritten.push_str(&render(c, &got));
            if update {
                continue;
            }
            let mut got_text = String::new();
            for (n, b) in &got {
                let _ = write!(got_text, "-- {n}\n{b}");
            }
            let mut want_text = String::new();
            for (n, b) in &c.logical {
                let _ = write!(want_text, "-- {n}\n{b}");
            }
            if got_text != want_text {
                let _ = write!(
                    failures,
                    "\ncase {} ({})\n{}",
                    c.name,
                    path.file_name().and_then(|f| f.to_str()).unwrap_or(""),
                    diff(&c.name, &want_text, &got_text)
                );
            }
        }
        if update {
            std::fs::write(&path, rewritten).expect("write golden");
        }
    }
    assert!(n_cases > 0, "no plan_golden cases found");
    assert!(failures.is_empty(), "plan_golden differences:{failures}");
}
