//! 正規表現エンジンの差分コーパス（`tests/data/regex_psql.tsv`。PostgreSQL 17 で期待値を作ったもの）。
//! 列は `pattern<TAB>flags<TAB>subject<TAB>expected`（COPY のテキスト形式）。`flags` は空か `i`（`~*`）、
//! `expected` は `t` / `f` / `ERR:<SQLSTATE>:<メッセージ>`。生成は `tests/data/gen_regex_corpus.sh`。
//!
//! PG が受け付けるが yuzhu が `0A000` を返す構文（先読み・後読み・後方参照・`(?b)` `(?e)`）は
//! 既知の未対応として許す（09 §9.2、D-9-7）。

use yuzhu_core::types::regex::regex_match;

fn unescape(s: &str) -> String {
    let mut out = String::new();
    let mut it = s.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('t') => out.push('\t'),
            Some('n') => out.push('\n'),
            Some('r') => out.push('\r'),
            Some(x) => out.push(x),
            None => out.push('\\'),
        }
    }
    out
}

#[test]
fn matches_postgresql_17() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../../../tests/data/regex_psql.tsv"
    );
    let text = std::fs::read_to_string(path).expect("regex_psql.tsv");
    let mut checked = 0;
    let mut unsupported = 0;
    let mut failures = Vec::new();
    for line in text.lines() {
        let f: Vec<&str> = line.split('\t').collect();
        assert_eq!(f.len(), 4, "bad row: {line:?}");
        let (pattern, subject) = (unescape(f[0]), unescape(f[2]));
        let want = unescape(f[3]);
        let got = match regex_match(&subject, &pattern, f[1] == "i") {
            Ok(b) => (if b { "t" } else { "f" }).to_owned(),
            Err(e) if e.sqlstate.code() == "0A000" => {
                unsupported += 1;
                continue;
            }
            Err(e) => format!("ERR:{}:{}", e.sqlstate.code(), e.message),
        };
        checked += 1;
        if got != want {
            failures.push(format!(
                "{pattern:?} ({}) ~ {subject:?}: want {want}, got {got}",
                f[1]
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
    assert!(checked > 2000, "only {checked} rows checked");
    assert!(unsupported < 20, "{unsupported} unsupported rows");
}

#[test]
fn redos_in_corpus_scale_is_fast() {
    let start = std::time::Instant::now();
    let subject = format!("{}!", "a".repeat(5000));
    assert!(!regex_match(&subject, "^(a+)+$", false).unwrap());
    assert!(!regex_match(&subject, "(a*)*b", false).unwrap());
    assert!(start.elapsed() < std::time::Duration::from_millis(1000));
}
