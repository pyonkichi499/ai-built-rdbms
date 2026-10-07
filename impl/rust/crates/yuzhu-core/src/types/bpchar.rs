//! `char(n)`（bpchar）の入出力・サイズ変換・比較・関数。担当 T2（`m4/09-types-functions.md` §3）。
//!
//! 値は `Datum::BpChar(String)`（パディング後の文字列）。比較とハッシュは末尾の空白を無視する
//! （`cmp_datum`）。`bpchar` と `text` の取り違えは `Datum` の変種で防ぐ。

use super::{Datum, io, ops};
use crate::error::{Error, Result, sqlstate};

/// `char(n)` の n の上限（`MaxAttrSize`）。
pub const MAX_BPCHAR_LEN: i64 = 10_485_760;

/// ディスク上のペイロード（UTF-8 のバイト列。パディング後。varlena ヘッダは `tuple.rs`）。
pub fn encode_bpchar(s: &str, out: &mut Vec<u8>) {
    out.extend_from_slice(s.as_bytes());
}

/// ペイロードから値を復元する。UTF-8 でなければ `XX001`。
pub fn decode_bpchar(b: &[u8]) -> Result<String> {
    std::str::from_utf8(b)
        .map(str::to_owned)
        .map_err(|_| Error::corrupted("invalid UTF-8 in a stored bpchar value"))
}

/// `bpcharin`: 値をそのまま保持する（typmod は無視。空白も保持）。
pub fn bpchar_in(s: &str) -> Datum {
    Datum::BpChar(s.to_owned())
}

/// 末尾の空白（U+0020 だけ）を落とす（`rtrim1`、`bcTruelen`）。
pub fn rtrim_spaces(s: &str) -> &str {
    s.trim_end_matches(' ')
}

/// `bpchar(bpchar, int4, bool)`: `char(n)` の長さ強制。文字数で数える。
///
/// - `typmod < 4`（`bpchar` の長さなし）: そのまま。
/// - n より短ければ右を空白で埋める。
/// - n より長ければ、明示キャストは黙って切り詰め、代入は超過部分がすべて空白のときだけ切り詰める
///   （そうでなければ `22001 value too long for type character(n)`）。
pub fn bpchar_coerce(s: String, typmod: i32, explicit: bool) -> Result<String> {
    if typmod < super::VARHDRSZ {
        return Ok(s);
    }
    let max = usize::try_from(typmod - super::VARHDRSZ).unwrap_or(0);
    let nchars = s.chars().count();
    let mut s = s;
    match nchars.cmp(&max) {
        std::cmp::Ordering::Greater => {
            let cut = s.char_indices().nth(max).map_or(s.len(), |(i, _)| i);
            if !explicit && !s[cut..].chars().all(|c| c == ' ') {
                return Err(Error::new(
                    sqlstate::STRING_DATA_RIGHT_TRUNCATION,
                    format!("value too long for type character({max})"),
                ));
            }
            s.truncate(cut);
        }
        std::cmp::Ordering::Less => s.extend(std::iter::repeat_n(' ', max - nchars)),
        std::cmp::Ordering::Equal => {}
    }
    Ok(s)
}

fn bad_arg(what: &str) -> Error {
    Error::internal(format!("unexpected argument for built-in {what}"))
}

/// bpchar と text のどちらでも文字列として読む（LIKE・正規表現の左辺）。
fn str_arg<'a>(args: &'a [Datum], i: usize, what: &str) -> Result<&'a str> {
    match args.get(i) {
        Some(Datum::BpChar(s) | Datum::Text(s)) => Ok(s),
        _ => Err(bad_arg(what)),
    }
}

// ---------------------------------------------------------------------------
// キャスト（pg_cast）
// ---------------------------------------------------------------------------

/// `text` / `varchar` → `bpchar`（PG ではバイナリ互換。`Datum` の変種だけが変わる）。
pub fn text_to_bpchar(args: &[Datum]) -> Result<Datum> {
    str_arg(args, 0, "bpchar cast").map(|s| Datum::BpChar(s.to_owned()))
}

/// `bpchar` → `text` / `varchar`（castfunc 401）: 末尾の空白を落とす。
pub fn bpchar_to_text(args: &[Datum]) -> Result<Datum> {
    str_arg(args, 0, "bpchar cast").map(|s| Datum::Text(rtrim_spaces(s).to_owned()))
}

/// `bpchar` → `name`（castfunc 409）: 末尾の空白を落とし、63 バイトで切る。
pub fn bpchar_to_name(args: &[Datum]) -> Result<Datum> {
    str_arg(args, 0, "bpchar cast")
        .map(|s| Datum::Text(io::truncate_identifier(rtrim_spaces(s)).to_owned()))
}

/// `name` → `bpchar`（castfunc 408）。
pub fn name_to_bpchar(args: &[Datum]) -> Result<Datum> {
    text_to_bpchar(args)
}

/// `"char"` → `bpchar`（castfunc 860）: 1 文字（0 は空文字列）。
pub fn char_to_bpchar(args: &[Datum]) -> Result<Datum> {
    match ops::char_to_text(args)? {
        Datum::Text(s) => Ok(Datum::BpChar(s)),
        _ => Err(bad_arg("char cast")),
    }
}

/// `bpchar` → `"char"`（castfunc 944）: 先頭の 1 バイト。
pub fn bpchar_to_char(args: &[Datum]) -> Result<Datum> {
    let s = str_arg(args, 0, "bpchar cast")?;
    Ok(Datum::Char(s.as_bytes().first().copied().unwrap_or(0)))
}

/// `bool` → `bpchar`（castfunc 2971）: `true` / `false`（typmod は別の `CoerceTypmod` が適用する）。
pub fn bool_to_bpchar(args: &[Datum]) -> Result<Datum> {
    match ops::bool_to_text(args)? {
        Datum::Text(s) => Ok(Datum::BpChar(s)),
        _ => Err(bad_arg("bool cast")),
    }
}

// ---------------------------------------------------------------------------
// 関数
// ---------------------------------------------------------------------------

fn len_i32(n: usize) -> Datum {
    Datum::Int4(i32::try_from(n).unwrap_or(i32::MAX))
}

/// `length(bpchar)` / `char_length` / `character_length`: 末尾の空白を除いた文字数。
pub fn bpchar_length(args: &[Datum]) -> Result<Datum> {
    let s = str_arg(args, 0, "length")?;
    Ok(len_i32(rtrim_spaces(s).chars().count()))
}

/// `octet_length(bpchar)`: パディング込みのバイト数。
pub fn bpchar_octet_length(args: &[Datum]) -> Result<Datum> {
    let s = str_arg(args, 0, "octet_length")?;
    Ok(len_i32(s.len()))
}

// ---------------------------------------------------------------------------
// LIKE と正規表現（パディングした値のまま照合する）
// ---------------------------------------------------------------------------

fn like_impl(args: &[Datum], icase: bool) -> Result<bool> {
    let s = str_arg(args, 0, "bpchar LIKE")?;
    let p = str_arg(args, 1, "bpchar LIKE")?;
    let esc = match args.get(2) {
        Some(Datum::Text(e)) => ops::like_escape_char(e)?,
        _ => Some('\\'),
    };
    ops::like_match_escape(s, p, esc, icase)
}

/// `bpcharlike`（`~~`）。
pub fn bpcharlike(args: &[Datum]) -> Result<Datum> {
    like_impl(args, false).map(Datum::Bool)
}
/// `bpcharnlike`（`!~~`）。
pub fn bpcharnlike(args: &[Datum]) -> Result<Datum> {
    like_impl(args, false).map(|b| Datum::Bool(!b))
}
/// `bpchariclike`（`~~*`）。
pub fn bpchariclike(args: &[Datum]) -> Result<Datum> {
    like_impl(args, true).map(Datum::Bool)
}
/// `bpcharicnlike`（`!~~*`）。
pub fn bpcharicnlike(args: &[Datum]) -> Result<Datum> {
    like_impl(args, true).map(|b| Datum::Bool(!b))
}

fn regex_impl(args: &[Datum], icase: bool) -> Result<bool> {
    let s = str_arg(args, 0, "bpchar regex")?;
    let p = str_arg(args, 1, "bpchar regex")?;
    Ok(super::regex::Regex::new(p, icase)?.is_match(s))
}

/// `bpcharregexeq`（`~`）。
pub fn bpcharregexeq(args: &[Datum]) -> Result<Datum> {
    regex_impl(args, false).map(Datum::Bool)
}
/// `bpcharregexne`（`!~`）。
pub fn bpcharregexne(args: &[Datum]) -> Result<Datum> {
    regex_impl(args, false).map(|b| Datum::Bool(!b))
}
/// `bpcharicregexeq`（`~*`）。
pub fn bpcharicregexeq(args: &[Datum]) -> Result<Datum> {
    regex_impl(args, true).map(Datum::Bool)
}
/// `bpcharicregexne`（`!~*`）。
pub fn bpcharicregexne(args: &[Datum]) -> Result<Datum> {
    regex_impl(args, true).map(|b| Datum::Bool(!b))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bp(s: &str) -> Datum {
        Datum::BpChar(s.to_owned())
    }

    #[test]
    fn coerce_pads_truncates_and_checks() {
        // char(3) = typmod 7
        assert_eq!(bpchar_coerce("ab".into(), 7, false).unwrap(), "ab ");
        assert_eq!(bpchar_coerce("abc  ".into(), 7, false).unwrap(), "abc");
        assert_eq!(bpchar_coerce("abcdef".into(), 7, true).unwrap(), "abc");
        let e = bpchar_coerce("abcdef".into(), 7, false).unwrap_err();
        assert_eq!(e.sqlstate.code(), "22001");
        assert_eq!(e.message, "value too long for type character(3)");
        // typmod -1 は何もしない。
        assert_eq!(bpchar_coerce("ab  ".into(), -1, false).unwrap(), "ab  ");
        // 文字数で数える（é は 2 バイトだが 1 文字）。
        let s = bpchar_coerce("é".into(), 7, false).unwrap();
        assert_eq!(s, "é  ");
        assert_eq!(s.len(), 4);
    }

    #[test]
    fn disk_roundtrip() {
        let mut out = Vec::new();
        encode_bpchar("ab ", &mut out);
        assert_eq!(out, b"ab ");
        assert_eq!(decode_bpchar(&out).unwrap(), "ab ");
        let e = decode_bpchar(&[0xff, 0xfe]).unwrap_err();
        assert_eq!(e.sqlstate.code(), "XX001");
    }

    #[test]
    fn casts() {
        assert_eq!(
            text_to_bpchar(&[Datum::Text("a ".into())]).unwrap(),
            bp("a ")
        );
        assert_eq!(
            bpchar_to_text(&[bp("ab   ")]).unwrap(),
            Datum::Text("ab".into())
        );
        // U+0020 以外の空白は落とさない。
        assert_eq!(
            bpchar_to_text(&[bp("ab\t ")]).unwrap(),
            Datum::Text("ab\t".into())
        );
        let long = "x".repeat(70);
        assert_eq!(
            bpchar_to_name(&[bp(&format!("{long}   "))]).unwrap(),
            Datum::Text("x".repeat(63))
        );
        assert_eq!(char_to_bpchar(&[Datum::Char(b'a')]).unwrap(), bp("a"));
        assert_eq!(char_to_bpchar(&[Datum::Char(0)]).unwrap(), bp(""));
        assert_eq!(bpchar_to_char(&[bp("xyz")]).unwrap(), Datum::Char(b'x'));
        assert_eq!(bpchar_to_char(&[bp("")]).unwrap(), Datum::Char(0));
        assert_eq!(bool_to_bpchar(&[Datum::Bool(true)]).unwrap(), bp("true"));
    }

    #[test]
    fn length_functions() {
        assert_eq!(bpchar_length(&[bp("ab   ")]).unwrap(), Datum::Int4(2));
        assert_eq!(bpchar_length(&[bp("é  ")]).unwrap(), Datum::Int4(1));
        assert_eq!(bpchar_octet_length(&[bp("ab   ")]).unwrap(), Datum::Int4(5));
        assert_eq!(bpchar_octet_length(&[bp("é  ")]).unwrap(), Datum::Int4(4));
    }

    #[test]
    fn like_and_regex_keep_the_padding() {
        let t = |s: &str| Datum::Text(s.to_owned());
        assert_eq!(bpcharlike(&[bp("a "), t("a_")]).unwrap(), Datum::Bool(true));
        assert_eq!(bpcharlike(&[bp("a "), t("a")]).unwrap(), Datum::Bool(false));
        assert_eq!(bpcharnlike(&[bp("a "), t("a")]).unwrap(), Datum::Bool(true));
        assert_eq!(
            bpchariclike(&[bp("AB "), t("ab%")]).unwrap(),
            Datum::Bool(true)
        );
        assert_eq!(
            bpcharregexeq(&[bp("a  "), t("a$")]).unwrap(),
            Datum::Bool(false)
        );
        assert_eq!(
            bpcharregexeq(&[bp("a  "), t("a *$")]).unwrap(),
            Datum::Bool(true)
        );
        assert_eq!(
            bpcharregexne(&[bp("a  "), t("a$")]).unwrap(),
            Datum::Bool(true)
        );
        assert_eq!(
            bpcharicregexeq(&[bp("A"), t("^a$")]).unwrap(),
            Datum::Bool(true)
        );
        assert_eq!(
            bpcharicregexne(&[bp("A"), t("^a$")]).unwrap(),
            Datum::Bool(false)
        );
    }
}
