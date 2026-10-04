//! Text input/output of the system types (`"char"`, `oid`, `regproc`, `tid`,
//! `xid`, `cid`, `oidvector`, `pg_node_tree`) (`m2.md` §4.2).
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
    )
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
    let name = type_name(type_oid);
    let b = s.as_bytes();
    let r = strtoul(b, 0);
    if r.end == 0 {
        return Err(syntax_error(name, s));
    }
    if r.overflow {
        return Err(range_error(name, s));
    }
    if b[r.end..].iter().any(|c| !is_space(*c)) {
        return Err(syntax_error(name, s));
    }
    // Accept the value if it survives truncation to 32 bits after either
    // unsigned or signed extension (a leading minus sign is allowed).
    let low = u32::try_from(r.value & 0xffff_ffff).unwrap_or(0);
    let signed_ok = i64::from_ne_bytes(r.value.to_ne_bytes()) == i64::from(low.cast_signed());
    if r.value != u64::from(low) && !signed_ok {
        return Err(range_error(name, s));
    }
    Ok(low)
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

/// `oidvectorin`: whitespace-separated OIDs.
fn oidvector_in(s: &str) -> Result<Vec<u32>> {
    let mut out = Vec::new();
    for word in s.split(|c: char| c.is_ascii() && is_space(c as u8)) {
        if word.is_empty() {
            continue;
        }
        if out.len() >= OIDVECTOR_MAX {
            return Err(Error::new(
                sqlstate::INVALID_PARAMETER_VALUE,
                "oidvector has too many elements",
            ));
        }
        out.push(uint32_in(word, oid::OID)?);
    }
    Ok(out)
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
        assert!(v("1,2").is_err());
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
