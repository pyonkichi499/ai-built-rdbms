//! 既知の差の台帳（`tests/known-diffs.toml`）と、流さない spec の一覧（`skip.txt`）の読み込み。
//!
//! 台帳は `[[kd]]` の繰り返しで、ここでは `id` と `affects`（文字列の配列。複数行可）だけを読む。
//! TOML の完全なパーサは持たない（依存を増やさない）。

use std::collections::HashMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// 台帳の中身（必要な部分だけ）。
#[derive(Debug, Default, Clone)]
pub(crate) struct Ledger {
    pub(crate) ids: Vec<String>,
    pub(crate) affects: Vec<String>,
}

impl Ledger {
    pub(crate) fn load(path: &Path) -> io::Result<Self> {
        Ok(Self::parse(&fs::read_to_string(path)?))
    }

    pub(crate) fn parse(text: &str) -> Self {
        let mut ledger = Self::default();
        let mut in_kd = false;
        let mut in_affects = false;
        for raw in text.lines() {
            let line = strip_comment(raw);
            let line = line.trim();
            if line.starts_with('[') && !in_affects {
                in_kd = line == "[[kd]]";
                continue;
            }
            if !in_kd {
                continue;
            }
            if in_affects {
                ledger.affects.extend(quoted_strings(line));
                if line.contains(']') {
                    in_affects = false;
                }
            } else if let Some(rest) = key_value(line, "affects") {
                ledger.affects.extend(quoted_strings(rest));
                in_affects = !rest.contains(']');
            } else if let Some(rest) = key_value(line, "id") {
                ledger.ids.extend(quoted_strings(rest));
            }
        }
        ledger
    }

    /// `key`（例 `isolation/ported/foo`）が、台帳の `affects` のどれかに（glob で）一致するか。
    pub(crate) fn affects_test(&self, key: &str) -> bool {
        self.affects.iter().any(|g| glob_match(g, key))
    }
}

/// `key = value` の行なら value 側を返す。
fn key_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let rest = line.strip_prefix(key)?.trim_start();
    rest.strip_prefix('=').map(str::trim_start)
}

/// 文字列の外の `#` 以降を落とす。
fn strip_comment(line: &str) -> &str {
    let mut in_str = false;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_str = !in_str,
            '#' if !in_str => return &line[..i],
            _ => {}
        }
    }
    line
}

/// 行の中の `"..."` をすべて取り出す（エスケープは扱わない）。
fn quoted_strings(s: &str) -> Vec<String> {
    s.split('"')
        .enumerate()
        .filter(|(i, _)| i % 2 == 1)
        .map(|(_, p)| p.to_owned())
        .collect()
}

/// `*`（任意の文字列。`/` も含む）だけを持つ glob。
pub(crate) fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (mut pi, mut ti) = (0usize, 0usize);
    let mut star: Option<(usize, usize)> = None;
    while ti < t.len() {
        if pi < p.len() && p[pi] == '*' {
            star = Some((pi, ti));
            pi += 1;
        } else if pi < p.len() && p[pi] == t[ti] {
            pi += 1;
            ti += 1;
        } else if let Some((sp, st)) = star {
            pi = sp + 1;
            ti = st + 1;
            star = Some((sp, st + 1));
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|&c| c == '*')
}

/// spec のパスから台帳の `affects` で使うキーを作る。
///
/// `.../isolation/<rest>/specs/<name>.spec` は `isolation/<rest>/<name>`（`specs` の段は落とす）。
/// 例: `tests/isolation/ported/specs/foo.spec` は `isolation/ported/foo`、
/// `tests/isolation/specs/foo.spec` は `isolation/foo`。`isolation` ディレクトリの下にないときは
/// `isolation/<name>`。
pub(crate) fn affects_key(spec_path: &Path) -> String {
    let name = spec_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let comps: Vec<String> = spec_path
        .parent()
        .map(|p| {
            p.components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect()
        })
        .unwrap_or_default();
    let mut key = String::from("isolation");
    if let Some(pos) = comps.iter().rposition(|c| c == "isolation") {
        for c in &comps[pos + 1..] {
            if c != "specs" {
                key.push('/');
                key.push_str(c);
            }
        }
    }
    key.push('/');
    key.push_str(&name);
    key
}

/// 台帳の場所を探す。`explicit` があればそれ。なければ `start` から親をたどり、
/// `known-diffs.toml` または `tests/known-diffs.toml` が最初に見つかったもの。
pub(crate) fn find_ledger(explicit: Option<&Path>, start: &Path) -> Option<PathBuf> {
    if let Some(p) = explicit {
        return Some(p.to_path_buf());
    }
    let start = fs::canonicalize(start).unwrap_or_else(|_| start.to_path_buf());
    start.ancestors().find_map(|dir| {
        [
            dir.join("known-diffs.toml"),
            dir.join("tests/known-diffs.toml"),
        ]
        .into_iter()
        .find(|c| c.is_file())
    })
}

/// `skip.txt` の 1 行（`<name> <KD-n> <理由>`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SkipEntry {
    pub(crate) kd: String,
    pub(crate) reason: String,
}

/// `skip.txt` を読む。空行と `#` で始まる行は無視する。KD を持たない行はエラー。
pub(crate) fn parse_skip_list(text: &str) -> Result<HashMap<String, SkipEntry>, String> {
    let mut map = HashMap::new();
    for (n, raw) in text.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut it = line.splitn(3, char::is_whitespace);
        let name = it.next().unwrap_or_default();
        let kd = it.next().unwrap_or_default().trim();
        let reason = it.next().unwrap_or_default().trim();
        if kd.is_empty() {
            return Err(format!(
                "skip list line {}: expected \"<name> <KD-n> <reason>\": {line}",
                n + 1
            ));
        }
        map.insert(
            name.to_owned(),
            SkipEntry {
                kd: kd.to_owned(),
                reason: reason.to_owned(),
            },
        );
    }
    Ok(map)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# 台帳
[[kd]]
id = "KD-1"
kind = "diff"
affects = ["isolation/ported/tuplelock-conflict", "isolation/ported/fk-deadlock", "slt/m5/fk/*"]   # 影響
note = "x"

[[kd]]
id = "KD-5"
affects = [
  "isolation/ported/deadlock-soft",   # コメント
  "isolation/ported/deadlock-soft-2",
]

[[kd]]
id = "KD-9"
"#;

    #[test]
    fn parses_ids_and_affects() {
        let l = Ledger::parse(SAMPLE);
        assert_eq!(l.ids, ["KD-1", "KD-5", "KD-9"]);
        assert!(l.affects_test("isolation/ported/fk-deadlock"));
        assert!(l.affects_test("isolation/ported/deadlock-soft-2"));
        assert!(l.affects_test("slt/m5/fk/a/b"));
        assert!(!l.affects_test("isolation/ported/unknown"));
        assert!(!l.affects_test("isolation/ported/deadlock"));
    }

    #[test]
    fn globs() {
        assert!(glob_match("a*c", "abc"));
        assert!(glob_match("a*c", "ac"));
        assert!(glob_match("*", ""));
        assert!(glob_match("a*b*c", "aXbYbZc"));
        assert!(!glob_match("a*c", "abd"));
        assert!(!glob_match("abc", "abcd"));
    }

    #[test]
    fn keys_from_paths() {
        assert_eq!(
            affects_key(Path::new("tests/isolation/ported/specs/foo.spec")),
            "isolation/ported/foo"
        );
        assert_eq!(
            affects_key(Path::new("/r/tests/isolation/specs/bar.spec")),
            "isolation/bar"
        );
        assert_eq!(affects_key(Path::new("/tmp/x/baz.spec")), "isolation/baz");
    }

    #[test]
    fn skip_list_parsing() {
        let m = parse_skip_list(
            "# c\n\nsimple-write-skew KD-3 SERIALIZABLE が前提\ndeadlock-parallel\tKD-28\tx y\n",
        )
        .unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m["simple-write-skew"].kd, "KD-3");
        assert_eq!(m["simple-write-skew"].reason, "SERIALIZABLE が前提");
        assert_eq!(m["deadlock-parallel"].reason, "x y");
        assert!(parse_skip_list("lonely\n").is_err());
    }

    #[test]
    fn finds_ledger_upwards() {
        let base = std::env::temp_dir().join(format!("yz-iso-ledger-{}", std::process::id()));
        let deep = base.join("tests/isolation/ported/specs");
        fs::create_dir_all(&deep).unwrap();
        assert_eq!(
            find_ledger(Some(Path::new("/x/y.toml")), &deep),
            Some("/x/y.toml".into())
        );
        fs::write(base.join("tests/known-diffs.toml"), "").unwrap();
        let found = find_ledger(None, &deep).unwrap();
        assert!(found.ends_with("tests/known-diffs.toml"), "{found:?}");
        fs::remove_dir_all(&base).unwrap();
    }
}
