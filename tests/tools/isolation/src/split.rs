//! ステップの SQL を文に分ける（`--protocol extended` で、文ごとに `Parse` / `Bind` / `Execute`
//! を送るため）。`spec.rs` のステップ区切りとは別の、SQL の字句だけを見る簡易スキャナ。
//!
//! `;` が区切りでない場所: 単一引用符の文字列（`''`。`E'...'` は `\` のエスケープも）、
//! 二重引用符の識別子（`""`）、ドル引用符（`$tag$ ... $tag$`）、`--` から行末、`/* ... */`（入れ子可）。

/// 区切りの結果。閉じていない引用符・コメントがあれば `None`。
///
/// 戻り値の各文は元の SQL のスライス（前後の空白は除く）。空白とコメントだけの文は捨てる。
pub(crate) fn split_statements_checked(sql: &str) -> Option<Vec<&str>> {
    let b = sql.as_bytes();
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut has_content = false;
    let mut i = 0usize;
    while i < b.len() {
        match b[i] {
            b';' => {
                push_stmt(&mut out, &sql[start..i], has_content);
                has_content = false;
                start = i + 1;
                i += 1;
            }
            b'\'' => {
                let escape = i > 0 && matches!(b[i - 1], b'E' | b'e') && !prev_is_ident(b, i - 1);
                i = skip_quoted(b, i, b'\'', escape)?;
                has_content = true;
            }
            b'"' => {
                i = skip_quoted(b, i, b'"', false)?;
                has_content = true;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = b[i..]
                    .iter()
                    .position(|&c| c == b'\n')
                    .map_or(b.len(), |p| i + p);
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = skip_block_comment(b, i)?;
            }
            b'$' if !prev_is_ident(b, i) => {
                if let Some(tag_end) = dollar_tag_end(b, i) {
                    let tag = &sql[i..tag_end];
                    let close = sql[tag_end..].find(tag)?;
                    i = tag_end + close + tag.len();
                } else {
                    i += 1;
                }
                has_content = true;
            }
            c => {
                if !c.is_ascii_whitespace() {
                    has_content = true;
                }
                i += 1;
            }
        }
    }
    push_stmt(&mut out, &sql[start..], has_content);
    Some(out)
}

/// 文に分ける。閉じていない引用符・コメントがあれば全体を 1 つの文として返す
/// （実行側は [`split_statements_checked`] で検出して Simple で送る）。
#[cfg(test)]
pub(crate) fn split_statements(sql: &str) -> Vec<&str> {
    split_statements_checked(sql).unwrap_or_else(|| vec![sql])
}

fn push_stmt<'a>(out: &mut Vec<&'a str>, s: &'a str, has_content: bool) {
    if has_content {
        let t = s.trim();
        if !t.is_empty() {
            out.push(t);
        }
    }
}

/// `b[i]` の直前が識別子の一部（英数字・`_`・`$`・非 ASCII）か。
fn prev_is_ident(b: &[u8], i: usize) -> bool {
    i > 0
        && (b[i - 1].is_ascii_alphanumeric() || matches!(b[i - 1], b'_' | b'$') || b[i - 1] >= 0x80)
}

/// `b[open]` が開き引用符 `q`。閉じ引用符の次の位置を返す。閉じていなければ `None`。
fn skip_quoted(b: &[u8], open: usize, q: u8, backslash: bool) -> Option<usize> {
    let mut i = open + 1;
    while i < b.len() {
        match b[i] {
            b'\\' if backslash => i += 2,
            c if c == q => {
                if b.get(i + 1) == Some(&q) {
                    i += 2;
                } else {
                    return Some(i + 1);
                }
            }
            _ => i += 1,
        }
    }
    None
}

/// `b[open..]` が `/*`。入れ子を数えて、対応する `*/` の次の位置を返す。
fn skip_block_comment(b: &[u8], open: usize) -> Option<usize> {
    let mut depth = 1usize;
    let mut i = open + 2;
    while i < b.len() {
        if b[i] == b'/' && b.get(i + 1) == Some(&b'*') {
            depth += 1;
            i += 2;
        } else if b[i] == b'*' && b.get(i + 1) == Some(&b'/') {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return Some(i);
            }
        } else {
            i += 1;
        }
    }
    None
}

/// `b[i]` が `$`。`$tag$`（タグは空でもよい。数字で始まるものは `$1` などのパラメータ）なら
/// 閉じ `$` の次の位置を返す。
fn dollar_tag_end(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    if j < b.len() && (b[j].is_ascii_alphabetic() || b[j] == b'_' || b[j] >= 0x80) {
        while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] >= 0x80) {
            j += 1;
        }
    }
    (b.get(j) == Some(&b'$')).then_some(j + 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_statements() {
        assert_eq!(
            split_statements("SELECT 1; SELECT 2"),
            ["SELECT 1", "SELECT 2"]
        );
        assert_eq!(split_statements("SELECT 1"), ["SELECT 1"]);
    }

    #[test]
    fn begin_update_commit_is_three() {
        assert_eq!(
            split_statements("BEGIN; UPDATE t SET s = 'a;b'; COMMIT;"),
            ["BEGIN", "UPDATE t SET s = 'a;b'", "COMMIT"]
        );
    }

    #[test]
    fn single_quotes_and_escapes() {
        assert_eq!(
            split_statements("SELECT 'it''s;'; SELECT 2"),
            ["SELECT 'it''s;'", "SELECT 2"]
        );
        assert_eq!(
            split_statements(r"SELECT E'a\';b'; SELECT 2"),
            [r"SELECT E'a\';b'", "SELECT 2"]
        );
        // E が付かなければ \ はエスケープでない。
        assert_eq!(
            split_statements(r"SELECT 'a\'; SELECT 2"),
            [r"SELECT 'a\'", "SELECT 2"]
        );
        // 識別子の末尾の e は E 文字列の接頭辞でない。
        assert_eq!(
            split_statements(r"SELECT une'a\'; SELECT 2"),
            [r"SELECT une'a\'", "SELECT 2"]
        );
    }

    #[test]
    fn double_quotes() {
        assert_eq!(
            split_statements(r#"SELECT "a;""b"; SELECT 2"#),
            [r#"SELECT "a;""b""#, "SELECT 2"]
        );
    }

    #[test]
    fn dollar_quotes() {
        assert_eq!(split_statements("SELECT $$;$$;"), ["SELECT $$;$$"]);
        assert_eq!(
            split_statements("DO $f$ BEGIN PERFORM 1; END $f$; SELECT 2"),
            ["DO $f$ BEGIN PERFORM 1; END $f$", "SELECT 2"]
        );
        // 別のタグの中の $$ は閉じではない。
        assert_eq!(
            split_statements("SELECT $a$ $$; $a$; SELECT 2"),
            ["SELECT $a$ $$; $a$", "SELECT 2"]
        );
        // パラメータ $1 はドル引用符ではない。
        assert_eq!(
            split_statements("SELECT $1; SELECT $2"),
            ["SELECT $1", "SELECT $2"]
        );
    }

    #[test]
    fn line_comments() {
        assert_eq!(split_statements("-- ;\nSELECT 1"), ["-- ;\nSELECT 1"]);
        assert_eq!(
            split_statements("SELECT 1 -- a;b\n; SELECT 2"),
            ["SELECT 1 -- a;b", "SELECT 2"]
        );
        assert_eq!(split_statements("SELECT 1; -- ;"), ["SELECT 1"]);
    }

    #[test]
    fn nested_block_comments() {
        assert_eq!(
            split_statements("SELECT /* a; /* b; */ c; */ 1; SELECT 2"),
            ["SELECT /* a; /* b; */ c; */ 1", "SELECT 2"]
        );
    }

    #[test]
    fn empty_statements_are_dropped() {
        assert_eq!(split_statements("").len(), 0);
        assert_eq!(split_statements("  \n\t ").len(), 0);
        assert_eq!(split_statements(";;  ; ").len(), 0);
        assert_eq!(split_statements("-- only a comment").len(), 0);
        assert_eq!(split_statements("/* c */ ; /* d */").len(), 0);
        assert_eq!(split_statements(";; SELECT 1 ;;"), ["SELECT 1"]);
    }

    #[test]
    fn trailing_whitespace_is_dropped() {
        assert_eq!(split_statements("SELECT 1;   \n  "), ["SELECT 1"]);
    }

    #[test]
    fn slices_borrow_from_input() {
        let sql = "SELECT 1; SELECT 2;";
        let base = sql.as_ptr() as usize;
        for s in split_statements(sql) {
            let p = s.as_ptr() as usize;
            assert!(p >= base && p + s.len() <= base + sql.len());
        }
    }

    #[test]
    fn unclosed_returns_whole() {
        for sql in [
            "SELECT 'a; SELECT 2",
            "SELECT \"a; SELECT 2",
            "SELECT $$a; SELECT 2",
            "SELECT /* a; SELECT 2",
            "SELECT /* /* */ ; SELECT 2",
            "SELECT E'a\\'; SELECT 2",
        ] {
            assert!(split_statements_checked(sql).is_none(), "{sql}");
            assert_eq!(split_statements(sql), [sql], "{sql}");
        }
    }

    #[test]
    fn non_ascii_is_kept() {
        assert_eq!(
            split_statements("SELECT '日本;語'; SELECT 'é'"),
            ["SELECT '日本;語'", "SELECT 'é'"]
        );
    }
}
