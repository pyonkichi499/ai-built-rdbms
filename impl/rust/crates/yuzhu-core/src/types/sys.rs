//! Text input/output of the system types (`"char"`, `oid`, `regproc`, `tid`,
//! `xid`, `cid`, `oidvector`, `pg_node_tree`) (`m2.md` §4.2), and of the M3
//! types `void` and `int4[]` (`m3.md` §6.10).
//!
//! The rules follow PostgreSQL 17 (`charin`/`charout`, `uint32in_subr`,
//! `tidin`, `oidvectorin`); the behaviour was checked against a real
//! PostgreSQL 17. `types` is the lowest layer, so names are not resolved
//! here: `regproc` input accepts numbers only, and the name shown for a
//! `regproc` value is produced by the executor's output stage.

use super::{Datum, Oid, SqlType, Tid, oid};
use crate::error::{Error, Result, sqlstate};

/// Maximum number of elements of an `oidvector` (`FUNC_MAX_ARGS`).
pub const OIDVECTOR_MAX: usize = 100;

/// Text output of a system-type value. `None` for NULL and for variants
/// that are not system types.
pub fn output_text(d: &Datum) -> Option<String> {
    Some(match d {
        Datum::Oid(v) | Datum::Xid(v) | Datum::Cid(v) => v.to_string(),
        Datum::Char(c) => char_out(*c),
        Datum::Tid(t) => format!("({},{})", t.block, t.offset),
        Datum::OidVector(v) => v.iter().map(u32::to_string).collect::<Vec<_>>().join(" "),
        Datum::Void => String::new(),
        Datum::Int4Array(v) => int4_array_out(v),
        Datum::Int2Vector(v) => v.iter().map(i16::to_string).collect::<Vec<_>>().join(" "),
        _ => return None,
    })
}

/// `charout`: the character itself; 0 gives an empty string; bytes of 0x80
/// and above are written as `\ooo` (octal, three digits).
fn char_out(c: u8) -> String {
    match c {
        0 => String::new(),
        1..=0x7f => char::from(c).to_string(),
        _ => format!("\\{c:03o}"),
    }
}

/// Input function of a system type. `ty.oid` is one of the types handled
/// here.
pub fn input_text(s: &str, ty: SqlType) -> Result<Datum> {
    match ty.oid {
        oid::CHAR => Ok(Datum::Char(char_in(s))),
        oid::OID => uint32_in(s, oid::OID).map(Datum::Oid),
        oid::XID => uint32_in(s, oid::XID).map(Datum::Xid),
        oid::CID => uint32_in(s, oid::CID).map(Datum::Cid),
        oid::REGPROC => regproc_in(s).map(Datum::Oid),
        oid::TID => tid_in(s).map(Datum::Tid),
        oid::OIDVECTOR => oidvector_in(s).map(Datum::OidVector),
        oid::VOID => Ok(Datum::Void),
        oid::INT4_ARRAY => int4_array_in(s).map(Datum::Int4Array),
        oid::INT2VECTOR => int2vector_in(s).map(Datum::Int2Vector),
        oid::INT2_ARRAY => int2_array_in(s).map(Datum::Int2Vector),
        oid::PG_NODE_TREE => Err(Error::new(
            sqlstate::FEATURE_NOT_SUPPORTED,
            "cannot accept a value of type pg_node_tree",
        )),
        other => Err(Error::internal(format!(
            "no input function for system type with OID {other}"
        ))),
    }
}

/// Whether `type_oid` is a type handled by this module.
pub fn handles(type_oid: Oid) -> bool {
    matches!(
        type_oid,
        oid::CHAR
            | oid::OID
            | oid::REGPROC
            | oid::TID
            | oid::XID
            | oid::CID
            | oid::OIDVECTOR
            | oid::PG_NODE_TREE
            | oid::VOID
            | oid::INT4_ARRAY
            | oid::INT2VECTOR
            | oid::INT2_ARRAY
    )
}

/// `array_out` for `int4[]`: `{1,2,NULL}`.
fn int4_array_out(v: &[Option<i32>]) -> String {
    let items: Vec<String> = v
        .iter()
        .map(|e| e.map_or_else(|| "NULL".to_owned(), |x| x.to_string()))
        .collect();
    format!("{{{}}}", items.join(","))
}

fn malformed_array(s: &str, detail: &str) -> Error {
    Error::new(
        sqlstate::INVALID_TEXT_REPRESENTATION,
        format!("malformed array literal: \"{s}\""),
    )
    .with_detail(detail)
}

/// Reads one array element starting at `*i`: quoted (`"..."`) or unquoted
/// (up to `,` or `}`, trailing whitespace dropped). Backslash escapes the
/// next byte. Returns the bytes and whether it was quoted.
fn read_array_element(s: &str, i: &mut usize) -> Result<(Vec<u8>, bool)> {
    let b = s.as_bytes();
    let end_of_input = || malformed_array(s, "Unexpected end of input.");
    let unexpected =
        |c: u8| malformed_array(s, &format!("Unexpected \"{}\" character.", char::from(c)));
    let mut buf = Vec::new();
    if b.get(*i) == Some(&b'"') {
        *i += 1;
        loop {
            match b.get(*i) {
                None => return Err(end_of_input()),
                Some(b'"') => {
                    *i += 1;
                    return Ok((buf, true));
                }
                Some(b'\\') => {
                    *i += 1;
                    buf.push(*b.get(*i).ok_or_else(end_of_input)?);
                    *i += 1;
                }
                Some(&x) => {
                    buf.push(x);
                    *i += 1;
                }
            }
        }
    }
    // Length of `buf` up to the last byte that is not trailing whitespace.
    let mut keep = 0;
    loop {
        match b.get(*i) {
            None => return Err(end_of_input()),
            Some(b',' | b'}') => break,
            Some(b'\\') => {
                *i += 1;
                buf.push(*b.get(*i).ok_or_else(end_of_input)?);
                keep = buf.len();
                *i += 1;
            }
            Some(&c @ (b'"' | b'{')) => return Err(unexpected(c)),
            Some(&x) => {
                buf.push(x);
                if !is_space(x) {
                    keep = buf.len();
                }
                *i += 1;
            }
        }
    }
    buf.truncate(keep);
    if buf.is_empty() {
        return Err(unexpected(b[*i]));
    }
    Ok((buf, false))
}

/// `array_in` for one-dimensional `int4[]`. An unquoted `NULL` (any case)
/// is NULL. Nested braces and explicit dimensions (`[1:2]={...}`) are not
/// supported (`0A000`).
fn int4_array_in(s: &str) -> Result<Vec<Option<i32>>> {
    let b = s.as_bytes();
    let skip_ws = |mut i: usize| {
        while i < b.len() && is_space(b[i]) {
            i += 1;
        }
        i
    };
    let end_of_input = || malformed_array(s, "Unexpected end of input.");

    let mut i = skip_ws(0);
    match b.get(i) {
        Some(b'{') => {}
        Some(b'[') => {
            return Err(Error::not_supported(
                "arrays with explicit dimensions are not supported",
            ));
        }
        _ => {
            return Err(malformed_array(
                s,
                "Array value must start with \"{\" or dimension information.",
            ));
        }
    }
    i = skip_ws(i + 1);
    let mut out = Vec::new();
    if b.get(i) == Some(&b'}') {
        i += 1;
    } else {
        loop {
            i = skip_ws(i);
            if b.get(i) == Some(&b'{') {
                return Err(Error::not_supported(
                    "multidimensional arrays are not supported",
                ));
            }
            let (bytes, quoted) = read_array_element(s, &mut i)?;
            let text = String::from_utf8(bytes)
                .map_err(|_| malformed_array(s, "Invalid element encoding."))?;
            if !quoted && text.eq_ignore_ascii_case("null") {
                out.push(None);
            } else {
                let v = super::io::int_in(&text, oid::INT4)?;
                out.push(Some(i32::try_from(v).expect("range checked by int_in")));
            }
            i = skip_ws(i);
            match b.get(i) {
                Some(b',') => i += 1,
                Some(b'}') => {
                    i += 1;
                    break;
                }
                Some(&x) => {
                    return Err(malformed_array(
                        s,
                        &format!("Unexpected \"{}\" character.", char::from(x)),
                    ));
                }
                None => return Err(end_of_input()),
            }
        }
    }
    if skip_ws(i) < b.len() {
        return Err(malformed_array(s, "Junk after closing right brace."));
    }
    Ok(out)
}

/// `charin`: `\ooo` (three octal digits) is that byte; otherwise the first
/// byte of the input (0 for an empty string).
pub fn char_in(s: &str) -> u8 {
    let b = s.as_bytes();
    if b.len() == 4 && b[0] == b'\\' && b[1..].iter().all(|c| (b'0'..=b'7').contains(c)) {
        let v =
            (u32::from(b[1] - b'0') << 6) | (u32::from(b[2] - b'0') << 3) | u32::from(b[3] - b'0');
        // Three octal digits can reach 0o777; the C code truncates to a byte.
        return u8::try_from(v & 0xff).unwrap_or(0);
    }
    b.first().copied().unwrap_or(0)
}

/// C `isspace` in the "C" locale.
fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

/// The result of a C `strtoul` call.
struct Strtoul {
    value: u64,
    /// Index of the first unparsed byte (0 when nothing was parsed).
    end: usize,
    overflow: bool,
}

/// C `strtoul(s, &end, base)` for `base` 10 or 0 (auto-detect `0x` / `0`).
/// A leading `-` negates the result modulo 2^64.
fn strtoul(b: &[u8], base: u32) -> Strtoul {
    let mut i = 0;
    while i < b.len() && is_space(b[i]) {
        i += 1;
    }
    let mut neg = false;
    if i < b.len() && (b[i] == b'+' || b[i] == b'-') {
        neg = b[i] == b'-';
        i += 1;
    }
    let mut radix = if base == 0 { 10 } else { base };
    if base == 0 && i < b.len() && b[i] == b'0' {
        let hex = b.get(i + 1).is_some_and(|c| matches!(c, b'x' | b'X'))
            && b.get(i + 2).is_some_and(u8::is_ascii_hexdigit);
        if hex {
            radix = 16;
            i += 2;
        } else {
            radix = 8;
        }
    }
    let start_digits = i;
    let mut value: u64 = 0;
    let mut overflow = false;
    while i < b.len() {
        let Some(d) = char::from(b[i]).to_digit(radix) else {
            break;
        };
        match value
            .checked_mul(u64::from(radix))
            .and_then(|v| v.checked_add(u64::from(d)))
        {
            Some(v) => value = v,
            None => overflow = true,
        }
        i += 1;
    }
    if i == start_digits {
        // "0x" without hex digits parses as "0"; handled by `radix = 8`
        // above. Nothing parsed at all.
        return Strtoul {
            value: 0,
            end: 0,
            overflow: false,
        };
    }
    if overflow {
        value = u64::MAX;
    } else if neg {
        value = value.wrapping_neg();
    }
    Strtoul {
        value,
        end: i,
        overflow,
    }
}

fn syntax_error(type_name: &str, s: &str) -> Error {
    Error::new(
        sqlstate::INVALID_TEXT_REPRESENTATION,
        format!("invalid input syntax for type {type_name}: \"{s}\""),
    )
}

fn range_error(type_name: &str, s: &str) -> Error {
    Error::new(
        sqlstate::NUMERIC_VALUE_OUT_OF_RANGE,
        format!("value \"{s}\" is out of range for type {type_name}"),
    )
}

/// The type name used in error messages.
fn type_name(type_oid: Oid) -> &'static str {
    match type_oid {
        oid::XID => "xid",
        oid::CID => "cid",
        oid::REGPROC => "regproc",
        _ => "oid",
    }
}

/// `uint32in_subr` (`oidin`, `xidin`, `cidin`): `strtoul` with base 0,
/// trailing whitespace allowed. Negative inputs down to -2^31 wrap to
/// their two's complement.
pub fn uint32_in(s: &str, type_oid: Oid) -> Result<u32> {
    let (value, end) = uint32_in_subr(s, type_oid)?;
    if s.as_bytes()[end..].iter().any(|c| !is_space(*c)) {
        return Err(syntax_error(type_name(type_oid), s));
    }
    Ok(value)
}

/// `uint32in_subr`: parses a number at the start of `s` and returns it with
/// the byte offset where parsing stopped (the caller checks the rest).
fn uint32_in_subr(s: &str, type_oid: Oid) -> Result<(u32, usize)> {
    let name = type_name(type_oid);
    let r = strtoul(s.as_bytes(), 0);
    if r.end == 0 {
        return Err(syntax_error(name, s));
    }
    if r.overflow {
        return Err(range_error(name, s));
    }
    // Accept the value if it survives truncation to 32 bits after either
    // unsigned or signed extension (a leading minus sign is allowed).
    let low = u32::try_from(r.value & 0xffff_ffff).unwrap_or(0);
    let signed_ok = i64::from_ne_bytes(r.value.to_ne_bytes()) == i64::from(low.cast_signed());
    if r.value != u64::from(low) && !signed_ok {
        return Err(range_error(name, s));
    }
    Ok((low, r.end))
}

/// `int2vectorin`: whitespace-separated `smallint`s (an empty string is the empty vector).
/// PostgreSQL 17 has no element-count limit here (checked against a real server).
pub fn int2vector_in(s: &str) -> Result<Vec<i16>> {
    s.split(|c: char| c.is_ascii() && is_space(c as u8))
        .filter(|t| !t.is_empty())
        .map(int2_element)
        .collect()
}

fn int2_element(tok: &str) -> Result<i16> {
    let v = super::io::int_in(tok, oid::INT2)?;
    i16::try_from(v).map_err(|_| range_error("smallint", tok))
}

/// `int2[]` input (`{1,2,3}`; only one dimension, no NULL elements).
pub fn int2_array_in(s: &str) -> Result<Vec<i16>> {
    int4_array_in(s)?
        .into_iter()
        .map(|e| match e {
            Some(v) => i16::try_from(v).map_err(|_| range_error("smallint", &v.to_string())),
            None => Err(Error::new(
                sqlstate::FEATURE_NOT_SUPPORTED,
                "NULL elements in int2[] are not supported yet",
            )),
        })
        .collect()
}

/// `array_out` for `int2[]`: `{1,2}` (the `int2vector` form is space separated).
pub fn int2_array_out(v: &[i16]) -> String {
    let items: Vec<String> = v.iter().map(i16::to_string).collect();
    format!("{{{}}}", items.join(","))
}

/// `regclassin` / `regtypein` / `regprocin` for the part that does not need names: digits only are an
/// OID as is (no existence check), `-` is 0. `None` means the caller must resolve a name.
fn reg_oid_literal(s: &str, type_name: &'static str) -> Result<Option<Oid>> {
    if s == "-" {
        return Ok(Some(0));
    }
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) {
        return s
            .parse::<u32>()
            .map(Some)
            .map_err(|_| range_error(type_name, s));
    }
    Ok(None)
}

fn reg_names<'a>(
    names: Option<&'a dyn super::OidNames>,
    what: &str,
) -> Result<&'a dyn super::OidNames> {
    names.ok_or_else(|| {
        Error::internal(format!(
            "{what} input requires a name lookup (TypeEnv.names)"
        ))
    })
}

/// `regclassin`. `names` resolves relation names (`CatalogNames`).
pub fn regclass_in(s: &str, names: Option<&dyn super::OidNames>) -> Result<Oid> {
    match reg_oid_literal(s, "oid")? {
        Some(o) => Ok(o),
        None => reg_names(names, "regclass")?.class_oid(s),
    }
}

/// `regtypein`. `names` resolves SQL type names (`CatalogNames`).
pub fn regtype_in(s: &str, names: Option<&dyn super::OidNames>) -> Result<Oid> {
    match reg_oid_literal(s, "oid")? {
        Some(o) => Ok(o),
        None => reg_names(names, "regtype")?.type_oid(s),
    }
}

/// `regnamespacein`. `names` resolves schema names (`CatalogNames`).
pub fn regnamespace_in(s: &str, names: Option<&dyn super::OidNames>) -> Result<Oid> {
    match reg_oid_literal(s, "oid")? {
        Some(o) => Ok(o),
        None => reg_names(names, "regnamespace")?.namespace_oid(s),
    }
}

/// `regprocin` for M2: a number, or `-` for 0. Names are not resolved at
/// this layer.
fn regproc_in(s: &str) -> Result<Oid> {
    if s.trim_matches(|c: char| c.is_ascii() && is_space(c as u8)) == "-" {
        return Ok(0);
    }
    let b = s.as_bytes();
    let first = b.iter().find(|c| !is_space(**c)).copied();
    if first.is_some_and(|c| c.is_ascii_digit() || c == b'+' || c == b'-') {
        return uint32_in(s, oid::REGPROC);
    }
    Err(Error::new(
        sqlstate::FEATURE_NOT_SUPPORTED,
        format!("regproc input of the name \"{s}\" is not supported yet"),
    )
    .with_hint("Use the numeric OID of the function."))
}

/// `tidin`: `(block,offset)`. Characters before the first `(` and after
/// the closing `)` are ignored, as in PostgreSQL.
#[allow(clippy::many_single_char_names)]
fn tid_in(s: &str) -> Result<Tid> {
    let err = || syntax_error("tid", s);
    let b = s.as_bytes();
    // Locate the start of the two coordinates.
    let mut coord: [Option<usize>; 2] = [None, None];
    let mut n = 0;
    let mut p = 0;
    while p < b.len() && n < 2 && b[p] != b')' {
        if b[p] == b',' || (b[p] == b'(' && n == 0) {
            coord[n] = Some(p + 1);
            n += 1;
        }
        p += 1;
    }
    let (Some(c0), Some(c1)) = (coord[0], coord[1]) else {
        return Err(err());
    };
    let r = strtoul(&b[c0..], 10);
    // `end == 0` means nothing was parsed: the next byte is at `c0`.
    let after = c0 + r.end;
    if r.overflow || b.get(after) != Some(&b',') {
        return Err(err());
    }
    let block = u32::try_from(r.value & 0xffff_ffff).unwrap_or(0);
    let signed_ok = i64::from_ne_bytes(r.value.to_ne_bytes()) == i64::from(block.cast_signed());
    if r.value != u64::from(block) && !signed_ok {
        return Err(err());
    }
    let r = strtoul(&b[c1..], 10);
    let after = c1 + r.end;
    if r.overflow || b.get(after) != Some(&b')') {
        return Err(err());
    }
    let offset = u16::try_from(r.value).map_err(|_| err())?;
    Ok(Tid { block, offset })
}

/// `oidvectorin`: whitespace-separated OIDs. As in PostgreSQL, each element
/// is parsed from the rest of the input, so errors quote that rest.
fn oidvector_in(s: &str) -> Result<Vec<u32>> {
    let mut out = Vec::new();
    let mut rest = s;
    loop {
        rest = rest.trim_start_matches(|c: char| c.is_ascii() && is_space(c as u8));
        if rest.is_empty() {
            return Ok(out);
        }
        if out.len() >= OIDVECTOR_MAX {
            return Err(Error::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                "oidvector has too many elements",
            ));
        }
        let (value, end) = uint32_in_subr(rest, oid::OID)?;
        out.push(value);
        rest = &rest[end..];
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ty(o: Oid) -> SqlType {
        SqlType::of(o)
    }

    fn oid_in(s: &str) -> Result<Datum> {
        input_text(s, ty(oid::OID))
    }

    fn arr(s: &str) -> Result<Vec<Option<i32>>> {
        match input_text(s, ty(oid::INT4_ARRAY))? {
            Datum::Int4Array(v) => Ok(v),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn int4_array_input() {
        assert_eq!(arr("{}").unwrap(), vec![]);
        assert_eq!(arr(" { } ").unwrap(), vec![]);
        assert_eq!(arr("{1,2,NULL}").unwrap(), vec![Some(1), Some(2), None]);
        assert_eq!(
            arr("{ 1 , -2 ,nUll }").unwrap(),
            vec![Some(1), Some(-2), None]
        );
        assert_eq!(arr("{\"1\", \"\\2\"}").unwrap(), vec![Some(1), Some(2)]);
        assert_eq!(arr("{1} ").unwrap(), vec![Some(1)]);
        // A quoted NULL is the string "NULL", not an integer.
        assert_eq!(
            arr("{\"NULL\"}").unwrap_err().sqlstate,
            sqlstate::INVALID_TEXT_REPRESENTATION
        );
    }

    #[test]
    fn int4_array_input_errors() {
        let malformed = |s: &str, detail: &str| {
            let e = arr(s).unwrap_err();
            assert_eq!(e.sqlstate, sqlstate::INVALID_TEXT_REPRESENTATION, "{s}");
            assert_eq!(e.message, format!("malformed array literal: \"{s}\""));
            assert_eq!(e.detail.as_deref(), Some(detail), "{s}");
        };
        malformed("{1,2", "Unexpected end of input.");
        malformed("{", "Unexpected end of input.");
        let start = "Array value must start with \"{\" or dimension information.";
        malformed("abc", start);
        malformed("", start);
        malformed("{1,,2}", "Unexpected \",\" character.");
        malformed("{,}", "Unexpected \",\" character.");
        malformed("{1,}", "Unexpected \"}\" character.");
        malformed("{1,2}x", "Junk after closing right brace.");
        let e = arr("{a}").unwrap_err();
        assert_eq!(e.message, "invalid input syntax for type integer: \"a\"");
        let e = arr("{1 2}").unwrap_err();
        assert_eq!(e.message, "invalid input syntax for type integer: \"1 2\"");
        let e = arr("{99999999999}").unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::NUMERIC_VALUE_OUT_OF_RANGE);
        for s in ["{{1},{2}}", "[1:2]={1,2}"] {
            assert_eq!(
                arr(s).unwrap_err().sqlstate,
                sqlstate::FEATURE_NOT_SUPPORTED
            );
        }
    }

    #[test]
    fn void_and_array_output() {
        assert_eq!(output_text(&Datum::Void).as_deref(), Some(""));
        assert_eq!(
            output_text(&Datum::Int4Array(vec![Some(1), None, Some(-5)])).as_deref(),
            Some("{1,NULL,-5}")
        );
        assert_eq!(
            output_text(&Datum::Int4Array(vec![])).as_deref(),
            Some("{}")
        );
        // void accepts any input, as void_in does.
        assert_eq!(input_text("abc", ty(oid::VOID)).unwrap(), Datum::Void);
    }

    #[test]
    fn output_of_each_type() {
        assert_eq!(output_text(&Datum::Oid(26)).as_deref(), Some("26"));
        assert_eq!(
            output_text(&Datum::Xid(u32::MAX)).as_deref(),
            Some("4294967295")
        );
        assert_eq!(output_text(&Datum::Cid(0)).as_deref(), Some("0"));
        assert_eq!(
            output_text(&Datum::Tid(Tid {
                block: 0,
                offset: 1
            }))
            .as_deref(),
            Some("(0,1)")
        );
        assert_eq!(
            output_text(&Datum::OidVector(vec![23, 23])).as_deref(),
            Some("23 23")
        );
        assert_eq!(output_text(&Datum::OidVector(vec![])).as_deref(), Some(""));
        assert_eq!(output_text(&Datum::Int4(1)), None);
        assert!(handles(oid::TID));
        assert!(!handles(oid::INT4));
    }

    #[test]
    fn char_output_and_input() {
        assert_eq!(output_text(&Datum::Char(b'r')).as_deref(), Some("r"));
        assert_eq!(output_text(&Datum::Char(0)).as_deref(), Some(""));
        assert_eq!(output_text(&Datum::Char(0xc3)).as_deref(), Some("\\303"));
        assert_eq!(char_in("abc"), b'a');
        assert_eq!(char_in(""), 0);
        assert_eq!(char_in("\\101"), b'A');
        assert_eq!(char_in("\\"), b'\\');
        assert_eq!(char_in("\\18"), b'\\');
        // The first byte of a multi-byte character.
        assert_eq!(char_in("é"), 0xc3);
        assert_eq!(input_text("x", ty(oid::CHAR)).unwrap(), Datum::Char(b'x'));
    }

    #[test]
    fn oid_boundaries() {
        assert_eq!(oid_in("4294967295").unwrap(), Datum::Oid(u32::MAX));
        assert_eq!(oid_in("-1").unwrap(), Datum::Oid(u32::MAX));
        assert_eq!(oid_in("-5").unwrap(), Datum::Oid(4_294_967_291));
        assert_eq!(oid_in("-2147483648").unwrap(), Datum::Oid(2_147_483_648));
        assert_eq!(oid_in(" 12 ").unwrap(), Datum::Oid(12));
        assert_eq!(oid_in("+5").unwrap(), Datum::Oid(5));
        assert_eq!(oid_in("0x10").unwrap(), Datum::Oid(16));
        assert_eq!(oid_in("010").unwrap(), Datum::Oid(8));
        assert_eq!(oid_in("0").unwrap(), Datum::Oid(0));
        for bad in ["4294967296", "-2147483649", "99999999999999999999999"] {
            let e = oid_in(bad).unwrap_err();
            assert_eq!(e.sqlstate, sqlstate::NUMERIC_VALUE_OUT_OF_RANGE, "{bad}");
        }
        assert_eq!(
            oid_in("4294967296").unwrap_err().message,
            "value \"4294967296\" is out of range for type oid"
        );
        for bad in ["abc", "", " ", "1.5", "12abc", "1_0", "0x"] {
            let e = oid_in(bad).unwrap_err();
            assert_eq!(e.sqlstate, sqlstate::INVALID_TEXT_REPRESENTATION, "{bad:?}");
        }
        assert_eq!(
            oid_in("abc").unwrap_err().message,
            "invalid input syntax for type oid: \"abc\""
        );
    }

    #[test]
    fn xid_and_cid_input() {
        assert_eq!(input_text("12", ty(oid::XID)).unwrap(), Datum::Xid(12));
        assert_eq!(
            input_text("-1", ty(oid::XID)).unwrap(),
            Datum::Xid(u32::MAX)
        );
        assert_eq!(input_text(" 3 ", ty(oid::CID)).unwrap(), Datum::Cid(3));
        let e = input_text("4294967296", ty(oid::XID)).unwrap_err();
        assert_eq!(
            e.message,
            "value \"4294967296\" is out of range for type xid"
        );
        let e = input_text("x", ty(oid::CID)).unwrap_err();
        assert_eq!(e.message, "invalid input syntax for type cid: \"x\"");
    }

    #[test]
    fn regproc_input_is_numeric_only() {
        assert_eq!(input_text("42", ty(oid::REGPROC)).unwrap(), Datum::Oid(42));
        assert_eq!(input_text("0", ty(oid::REGPROC)).unwrap(), Datum::Oid(0));
        assert_eq!(input_text("-", ty(oid::REGPROC)).unwrap(), Datum::Oid(0));
        let e = input_text("int4in", ty(oid::REGPROC)).unwrap_err();
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
    }

    #[test]
    fn tid_input() {
        let t = |s: &str| input_text(s, ty(oid::TID));
        let want = |block, offset| Datum::Tid(Tid { block, offset });
        assert_eq!(t("(1,2)").unwrap(), want(1, 2));
        assert_eq!(t("(1, 2)").unwrap(), want(1, 2));
        assert_eq!(t(" (1,2) x").unwrap(), want(1, 2));
        assert_eq!(t("(0,0)").unwrap(), want(0, 0));
        assert_eq!(t("(4294967295,65535)").unwrap(), want(u32::MAX, u16::MAX));
        assert_eq!(t("(-1,1)").unwrap(), want(u32::MAX, 1));
        for bad in [
            "(4294967296,1)",
            "(1,65536)",
            "1,2",
            "( 1 , 2 )",
            "(1,2",
            "(0x10,1)",
            "",
            "abc",
        ] {
            let e = t(bad).unwrap_err();
            assert_eq!(e.sqlstate, sqlstate::INVALID_TEXT_REPRESENTATION, "{bad:?}");
        }
        assert_eq!(
            t("1,2").unwrap_err().message,
            "invalid input syntax for type tid: \"1,2\""
        );
    }

    #[test]
    fn oidvector_input() {
        let v = |s: &str| input_text(s, ty(oid::OIDVECTOR));
        assert_eq!(v("1 2").unwrap(), Datum::OidVector(vec![1, 2]));
        assert_eq!(v("1  2 ").unwrap(), Datum::OidVector(vec![1, 2]));
        assert_eq!(v("").unwrap(), Datum::OidVector(vec![]));
        assert_eq!(v("0x10 5").unwrap(), Datum::OidVector(vec![16, 5]));
        assert_eq!(v("-1").unwrap(), Datum::OidVector(vec![u32::MAX]));
        let e = v("1 a").unwrap_err();
        assert_eq!(e.message, "invalid input syntax for type oid: \"a\"");
        assert_eq!(
            v("1,2").unwrap_err().message,
            "invalid input syntax for type oid: \",2\""
        );
        assert_eq!(
            v("1 2x 3").unwrap_err().message,
            "invalid input syntax for type oid: \"x 3\""
        );
        assert_eq!(
            v("1 4294967296 3").unwrap_err().message,
            "value \"4294967296 3\" is out of range for type oid"
        );
        let many = vec!["1"; OIDVECTOR_MAX + 1].join(" ");
        assert!(v(&many).is_err());
        assert!(v(&many[..many.len() - 2]).is_ok());
    }

    #[test]
    fn pg_node_tree_has_no_input() {
        let e = input_text("x", ty(oid::PG_NODE_TREE)).unwrap_err();
        assert_eq!(e.message, "cannot accept a value of type pg_node_tree");
    }
}

#[cfg(test)]
mod reg_tests {
    use super::*;
    use crate::types::OidNames;

    #[derive(Debug)]
    struct Names;

    impl OidNames for Names {
        fn class_oid(&self, name: &str) -> Result<Oid> {
            match name {
                "pg_class" => Ok(1259),
                _ => Err(Error::new(
                    sqlstate::UNDEFINED_TABLE,
                    format!("relation \"{name}\" does not exist"),
                )),
            }
        }
        fn class_name(&self, _: Oid) -> Option<String> {
            None
        }
        fn type_oid(&self, name: &str) -> Result<Oid> {
            if name == "int4" {
                Ok(23)
            } else {
                Err(Error::internal("no"))
            }
        }
        fn type_name(&self, _: Oid) -> Option<String> {
            None
        }
        fn proc_name(&self, _: Oid) -> Option<String> {
            None
        }
    }

    #[test]
    fn regclass_literals() {
        let n: &dyn OidNames = &Names;
        assert_eq!(regclass_in("1259", Some(n)).unwrap(), 1259);
        assert_eq!(regclass_in("99999999", None).unwrap(), 99_999_999);
        assert_eq!(regclass_in("-", None).unwrap(), 0);
        assert_eq!(regclass_in("0", None).unwrap(), 0);
        assert_eq!(regclass_in("pg_class", Some(n)).unwrap(), 1259);
        assert_eq!(
            regclass_in("nosuch", Some(n)).unwrap_err().sqlstate.code(),
            "42P01"
        );
        assert_eq!(
            regclass_in("pg_class", None).unwrap_err().sqlstate.code(),
            "XX000"
        );
        let e = regclass_in("99999999999", Some(n)).unwrap_err();
        assert_eq!(e.sqlstate.code(), "22003");
        assert_eq!(
            e.message,
            "value \"99999999999\" is out of range for type oid"
        );
        assert_eq!(regtype_in("23", None).unwrap(), 23);
        assert_eq!(regtype_in("int4", Some(n)).unwrap(), 23);
    }

    #[test]
    fn int2vector_and_int2_array() {
        assert_eq!(int2vector_in("1 2 3").unwrap(), vec![1, 2, 3]);
        assert_eq!(int2vector_in("").unwrap(), Vec::<i16>::new());
        assert_eq!(int2vector_in(" 1  2 ").unwrap(), vec![1, 2]);
        assert_eq!(int2vector_in("-5 +7").unwrap(), vec![-5, 7]);
        let many = (1..=101)
            .map(|i| i.to_string())
            .collect::<Vec<_>>()
            .join(" ");
        assert_eq!(int2vector_in(&many).unwrap().len(), 101);
        let e = int2vector_in("70000").unwrap_err();
        assert_eq!(
            (e.sqlstate.code(), e.message.as_str()),
            ("22003", "value \"70000\" is out of range for type smallint")
        );
        let e = int2vector_in("1 x").unwrap_err();
        assert_eq!(
            (e.sqlstate.code(), e.message.as_str()),
            ("22P02", "invalid input syntax for type smallint: \"x\"")
        );
        assert_eq!(int2vector_in("1,2").unwrap_err().sqlstate.code(), "22P02");
        assert_eq!(int2_array_in("{1,2,3}").unwrap(), vec![1, 2, 3]);
        assert_eq!(
            int2_array_in("{70000}").unwrap_err().sqlstate.code(),
            "22003"
        );
        assert_eq!(
            int2_array_in("{1,NULL}").unwrap_err().sqlstate.code(),
            "0A000"
        );
        assert_eq!(int2_array_out(&[1, 2]), "{1,2}");
        assert_eq!(
            output_text(&Datum::Int2Vector(vec![1, 2, 3])).as_deref(),
            Some("1 2 3")
        );
        assert_eq!(
            input_text("1 2", SqlType::INT2VECTOR).unwrap(),
            Datum::Int2Vector(vec![1, 2])
        );
        assert_eq!(
            input_text("{4,5}", SqlType::of(oid::INT2_ARRAY)).unwrap(),
            Datum::Int2Vector(vec![4, 5])
        );
    }

    #[test]
    fn char_empty_string_is_nul() {
        // relkind IN ('r','p','') の '' は "char" の 0 バイト（G-5）。
        assert_eq!(
            input_text("", SqlType::of(oid::CHAR)).unwrap(),
            Datum::Char(0)
        );
        assert_eq!(output_text(&Datum::Char(0)).as_deref(), Some(""));
    }
}
