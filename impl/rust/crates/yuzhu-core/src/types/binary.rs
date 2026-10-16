//! バイナリ形式の入出力の振り分け（XQ-4。`m5/04` §6.5）。
//!
//! `binary_send(d, ty)` は値を `*_send` 相当のバイト列にし、`binary_recv(buf, ty)` は `*_recv` 相当で
//! `RecvBuf` から値を読む。値の長さが決まっている型は `binary_recv` が必要な分だけ読み、余りの検査は
//! 呼び出し側が `RecvBuf::finish` で行う（`input_binary` は両方をまとめた入口）。
//!
//! numeric・日付時刻・bytea・uuid・bpchar・配列は TY-6 / TD-5 が持つ。それらは `deferred_send` /
//! `deferred_recv` の `match` に足す（今は `0A000`）。`supports_binary` も同じ表に合わせて更新する。

use super::array::ArrayValue;
use super::{Datum, MAX_IDENTIFIER_LENGTH, SqlType, Tid, oid};
use crate::error::{Error, Result, sqlstate};

/// 受信したバイト列を前から読む。短ければ `08P01`、`finish` で余りがあれば `22P03`。
#[derive(Debug)]
pub struct RecvBuf<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> RecvBuf<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.data.len() - self.pos
    }

    /// `n` バイトを取り出す。足りなければ `08P01 insufficient data left in message`。
    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.remaining() {
            return Err(Error::new(
                sqlstate::PROTOCOL_VIOLATION,
                "insufficient data left in message",
            ));
        }
        let s = &self.data[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn array<const N: usize>(&mut self) -> Result<[u8; N]> {
        let mut a = [0u8; N];
        a.copy_from_slice(self.take(N)?);
        Ok(a)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.array::<1>()?[0])
    }
    pub fn i16(&mut self) -> Result<i16> {
        Ok(i16::from_be_bytes(self.array()?))
    }
    pub fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_be_bytes(self.array()?))
    }
    pub fn i32(&mut self) -> Result<i32> {
        Ok(i32::from_be_bytes(self.array()?))
    }
    pub fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_be_bytes(self.array()?))
    }
    pub fn i64(&mut self) -> Result<i64> {
        Ok(i64::from_be_bytes(self.array()?))
    }

    /// 残り全部を取り出す。
    pub fn rest(&mut self) -> &'a [u8] {
        let s = &self.data[self.pos..];
        self.pos = self.data.len();
        s
    }

    /// 余りがあれば `22P03 incorrect binary data format`。
    pub fn finish(&self) -> Result<()> {
        if self.remaining() == 0 {
            Ok(())
        } else {
            Err(bad_format("incorrect binary data format"))
        }
    }
}

fn bad_format(msg: impl Into<String>) -> Error {
    Error::new(sqlstate::INVALID_BINARY_REPRESENTATION, msg)
}

/// この型にバイナリの入出力があるか（`pg_type.typsend` / `typreceive` と `Describe` の判断に使う）。
pub fn supports_binary(type_oid: u32) -> bool {
    matches!(
        type_oid,
        oid::BOOL
            | oid::CHAR
            | oid::NAME
            | oid::INT8
            | oid::INT2
            | oid::INT4
            | oid::FLOAT4
            | oid::FLOAT8
            | oid::TEXT
            | oid::VARCHAR
            | oid::UNKNOWN
            | oid::PG_NODE_TREE
            | oid::OID
            | oid::XID
            | oid::CID
            | oid::REGPROC
            | oid::REGCLASS
            | oid::REGTYPE
            | oid::TID
            | oid::INT2VECTOR
            | oid::OIDVECTOR
            | oid::DATE
            | oid::TIME
            | oid::TIMESTAMP
            | oid::TIMESTAMPTZ
            | oid::INTERVAL
    )
}

fn is_datetime(type_oid: u32) -> bool {
    matches!(
        type_oid,
        oid::DATE | oid::TIME | oid::TIMESTAMP | oid::TIMESTAMPTZ | oid::INTERVAL
    )
}

fn mismatch(d: &Datum, ty: SqlType) -> Error {
    Error::internal(format!(
        "binary_send: datum {d:?} does not match type oid {}",
        ty.oid
    ))
}

/// TY-6 / TD-5 が足す型の send。表に無い型は、バイナリの出力関数が無い（`42883`）。
fn deferred_send(d: &Datum, ty: SqlType) -> Result<Vec<u8>> {
    let _ = d;
    Err(no_binary(ty.oid, "output"))
}

/// TY-6 / TD-5 が足す型の recv。
fn deferred_recv(_buf: &mut RecvBuf<'_>, ty: SqlType) -> Result<Datum> {
    Err(no_binary(ty.oid, "input"))
}

fn no_binary(type_oid: u32, dir: &str) -> Error {
    Error::new(
        sqlstate::UNDEFINED_FUNCTION,
        format!("no binary {dir} function available for type oid {type_oid}"),
    )
}

/// NULL でない値を `*_send` の形式にする。NULL は呼び出し側が長さ −1 で出す。
pub fn binary_send(d: &Datum, ty: SqlType) -> Result<Vec<u8>> {
    Ok(match (ty.oid, d) {
        (oid::BOOL, Datum::Bool(b)) => vec![u8::from(*b)],
        (oid::CHAR, Datum::Char(c)) => vec![*c],
        (oid::INT2, Datum::Int2(v)) => v.to_be_bytes().to_vec(),
        (oid::INT4, Datum::Int4(v)) => v.to_be_bytes().to_vec(),
        (oid::INT8, Datum::Int8(v)) => v.to_be_bytes().to_vec(),
        (oid::FLOAT4, Datum::Float4(v)) => v.to_bits().to_be_bytes().to_vec(),
        (oid::FLOAT8, Datum::Float8(v)) => v.to_bits().to_be_bytes().to_vec(),
        (
            oid::TEXT | oid::VARCHAR | oid::UNKNOWN | oid::PG_NODE_TREE | oid::NAME,
            Datum::Text(s),
        ) => s.as_bytes().to_vec(),
        (oid::OID | oid::REGPROC | oid::REGCLASS | oid::REGTYPE, Datum::Oid(v)) => {
            v.to_be_bytes().to_vec()
        }
        (oid::XID, Datum::Xid(v)) | (oid::CID, Datum::Cid(v)) => v.to_be_bytes().to_vec(),
        (oid::TID, Datum::Tid(t)) => {
            let mut v = t.block.to_be_bytes().to_vec();
            v.extend_from_slice(&t.offset.to_be_bytes());
            v
        }
        (oid::INT2VECTOR, Datum::Int2Vector(items)) => {
            send_vector(oid::INT2, items.iter().map(|v| v.to_be_bytes().to_vec()))
        }
        (oid::OIDVECTOR, Datum::OidVector(items)) => {
            send_vector(oid::OID, items.iter().map(|v| v.to_be_bytes().to_vec()))
        }
        (o, _) if is_datetime(o) => return super::datetime::binary_send(d, ty),
        (o, _) if supports_binary(o) => return Err(mismatch(d, ty)),
        _ => return deferred_send(d, ty),
    })
}

/// 下限 0 の 1 次元配列（int2vector / oidvector）。空は `ndim 0`。
fn send_vector(elem: u32, items: impl ExactSizeIterator<Item = Vec<u8>>) -> Vec<u8> {
    let n = items.len();
    let mut out = Vec::new();
    out.extend_from_slice(&i32::from(n > 0).to_be_bytes());
    out.extend_from_slice(&0i32.to_be_bytes());
    out.extend_from_slice(&elem.to_be_bytes());
    if n > 0 {
        out.extend_from_slice(&i32::try_from(n).unwrap_or(i32::MAX).to_be_bytes());
        out.extend_from_slice(&0i32.to_be_bytes());
    }
    for it in items {
        out.extend_from_slice(&i32::try_from(it.len()).unwrap_or(i32::MAX).to_be_bytes());
        out.extend_from_slice(&it);
    }
    out
}

fn recv_text(buf: &mut RecvBuf<'_>) -> Result<String> {
    let bytes = buf.rest();
    match std::str::from_utf8(bytes) {
        Ok(s) if !s.contains('\0') => Ok(s.to_owned()),
        Ok(_) => Err(utf8_error(&[0])),
        Err(e) => {
            let at = e.valid_up_to();
            let end = (at + e.error_len().unwrap_or(1)).min(bytes.len());
            Err(utf8_error(&bytes[at..end]))
        }
    }
}

fn utf8_error(bad: &[u8]) -> Error {
    let hex: Vec<String> = bad.iter().map(|b| format!("0x{b:02x}")).collect();
    Error::new(
        sqlstate::CHARACTER_NOT_IN_REPERTOIRE,
        format!(
            "invalid byte sequence for encoding \"UTF8\": {}",
            hex.join(" ")
        ),
    )
}

/// `*_recv` 相当。固定長の型は必要な分だけ読む（余りは呼び出し側の `finish`）。
pub fn binary_recv(buf: &mut RecvBuf<'_>, ty: SqlType) -> Result<Datum> {
    Ok(match ty.oid {
        oid::BOOL => Datum::Bool(buf.u8()? != 0),
        oid::CHAR => Datum::Char(buf.u8()?),
        oid::INT2 => Datum::Int2(buf.i16()?),
        oid::INT4 => Datum::Int4(buf.i32()?),
        oid::INT8 => Datum::Int8(buf.i64()?),
        oid::FLOAT4 => Datum::Float4(f32::from_bits(buf.u32()?)),
        oid::FLOAT8 => Datum::Float8(f64::from_bits(buf.i64()?.cast_unsigned())),
        oid::TEXT | oid::VARCHAR | oid::UNKNOWN | oid::PG_NODE_TREE => Datum::Text(recv_text(buf)?),
        oid::NAME => {
            let s = recv_text(buf)?;
            if s.len() > MAX_IDENTIFIER_LENGTH {
                return Err(Error::new(sqlstate::NAME_TOO_LONG, "identifier too long")
                    .with_detail(format!(
                        "Identifier must be less than {} characters.",
                        MAX_IDENTIFIER_LENGTH + 1
                    )));
            }
            Datum::Text(s)
        }
        oid::OID | oid::REGPROC | oid::REGCLASS | oid::REGTYPE => Datum::Oid(buf.u32()?),
        oid::XID => Datum::Xid(buf.u32()?),
        oid::CID => Datum::Cid(buf.u32()?),
        oid::TID => Datum::Tid(Tid {
            block: buf.u32()?,
            offset: buf.u16()?,
        }),
        oid::INT2VECTOR => {
            let items = recv_vector(buf, oid::INT2, "int2vector", 2)?;
            Datum::Int2Vector(
                items
                    .iter()
                    .map(|b| i16::from_be_bytes([b[0], b[1]]))
                    .collect(),
            )
        }
        oid::OIDVECTOR => {
            let items = recv_vector(buf, oid::OID, "oidvector", 4)?;
            Datum::OidVector(
                items
                    .iter()
                    .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
                    .collect(),
            )
        }
        o if is_datetime(o) => return super::datetime::binary_recv(buf, ty),
        _ => return deferred_recv(buf, ty),
    })
}

/// int2vector / oidvector の `*_recv`。各要素のバイト列を返す。
fn recv_vector<'a>(
    buf: &mut RecvBuf<'a>,
    elem: u32,
    name: &str,
    width: usize,
) -> Result<Vec<&'a [u8]>> {
    let invalid = || bad_format(format!("invalid {name} data"));
    let ndim = buf.i32()?;
    if !(0..=6).contains(&ndim) {
        return Err(bad_format(format!("invalid number of dimensions: {ndim}")));
    }
    let flags = buf.i32()?;
    if flags != 0 && flags != 1 {
        return Err(bad_format(format!("invalid array flags: {flags}")));
    }
    if buf.u32()? != elem {
        return Err(bad_format("wrong element type"));
    }
    let mut dims = Vec::new();
    for _ in 0..ndim {
        dims.push((buf.i32()?, buf.i32()?));
    }
    if ndim > 1 || flags == 1 {
        return Err(invalid());
    }
    let mut out = Vec::new();
    if let Some(&(len, lbound)) = dims.first() {
        if len < 0 {
            return Err(bad_format("invalid array dimension"));
        }
        if lbound != 0 {
            return Err(invalid());
        }
        for i in 0..len {
            let l = buf.i32()?;
            if l == -1 {
                return Err(invalid());
            }
            if usize::try_from(l).ok() != Some(width) {
                return Err(bad_format(format!(
                    "improper binary format in array element {i}"
                )));
            }
            out.push(buf.take(width)?);
        }
    }
    Ok(out)
}

/// 1 つの値全体を読む。余りは `22P03`。
pub fn input_binary(bytes: &[u8], ty: SqlType) -> Result<Datum> {
    let mut buf = RecvBuf::new(bytes);
    let d = binary_recv(&mut buf, ty)?;
    buf.finish()?;
    Ok(d)
}

/// 配列値の型だけ参照するための再公開（TY-6 が `array.rs` の部品を使う口）。
pub type ArrayDatum = ArrayValue;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::cmp_datum;
    use proptest::prelude::*;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    fn fixed() -> Vec<(u32, Datum, Vec<u8>)> {
        vec![
            (oid::BOOL, Datum::Bool(true), hex("01")),
            (oid::INT2, Datum::Int2(-1), hex("ffff")),
            (oid::INT8, Datum::Int8(1), hex("0000000000000001")),
            (oid::FLOAT4, Datum::Float4(1.5), hex("3fc00000")),
            (
                oid::FLOAT4,
                Datum::Float4(f32::NEG_INFINITY),
                hex("ff800000"),
            ),
            (
                oid::FLOAT8,
                Datum::Float8(f64::NAN),
                hex("7ff8000000000000"),
            ),
            (
                oid::TID,
                Datum::Tid(Tid {
                    block: 1,
                    offset: 2,
                }),
                hex("000000010002"),
            ),
            (oid::REGCLASS, Datum::Oid(1259), hex("000004eb")),
            (
                oid::INT2VECTOR,
                Datum::Int2Vector(vec![1, 2]),
                hex("00000001 00000000 00000015 00000002 00000000 00000002 0001 00000002 0002"),
            ),
            (
                oid::INT2VECTOR,
                Datum::Int2Vector(vec![]),
                hex("00000000 00000000 00000015"),
            ),
            (
                oid::TEXT,
                Datum::Text("あa".into()),
                "あa".as_bytes().to_vec(),
            ),
            (oid::TEXT, Datum::Text(String::new()), vec![]),
        ]
    }

    #[test]
    fn fixed_values() {
        for (o, d, bytes) in fixed() {
            let ty = SqlType::of(o);
            assert_eq!(binary_send(&d, ty).unwrap(), bytes, "send {d:?}");
            let back = input_binary(&bytes, ty).unwrap();
            if let Datum::Float8(f) = &d {
                assert!(matches!(back, Datum::Float8(g) if g.is_nan() == f.is_nan()));
            } else {
                assert_eq!(cmp_datum(&back, &d), std::cmp::Ordering::Equal, "{d:?}");
            }
        }
    }

    fn code(r: Result<Datum>) -> String {
        r.unwrap_err().sqlstate.0.to_owned()
    }

    #[test]
    fn error_table() {
        let t = |o, b: &[u8]| code(input_binary(b, SqlType::of(o)));
        assert_eq!(t(oid::INT4, &[0; 5]), "22P03");
        assert_eq!(t(oid::INT4, &[0; 2]), "08P01");
        assert_eq!(t(oid::INT4, &[]), "08P01");
        assert_eq!(t(oid::INT2, &[0; 3]), "22P03");
        assert_eq!(t(oid::BOOL, &[1, 1]), "22P03");
        assert_eq!(t(oid::BOOL, &[]), "08P01");
        assert!(matches!(
            input_binary(&[2], SqlType::BOOL),
            Ok(Datum::Bool(true))
        ));
        assert_eq!(t(oid::FLOAT8, &[0; 7]), "08P01");
        assert_eq!(t(oid::TID, &[0; 5]), "08P01");
        assert_eq!(t(oid::TEXT, &[0xff]), "22021");
        assert_eq!(t(oid::TEXT, &[b'a', 0]), "22021");
        assert_eq!(t(oid::NAME, &[b'a'; 64]), "42622");
        assert!(input_binary(&[b'a'; 63], SqlType::NAME).is_ok());
        assert_eq!(t(1033, &[]), "42883");
    }

    #[test]
    fn int2vector_errors() {
        let t = |s: &str| code(input_binary(&hex(s), SqlType::INT2VECTOR));
        assert_eq!(
            t("00000001 00000000 00000017 00000001 00000000 00000002 0001"),
            "22P03"
        );
        // lbound 1
        assert_eq!(
            t("00000001 00000000 00000015 00000001 00000001 00000002 0001"),
            "22P03"
        );
        // NULL 要素
        assert_eq!(
            t("00000001 00000001 00000015 00000001 00000000 ffffffff"),
            "22P03"
        );
        // 要素の長さ違い
        assert_eq!(
            t("00000001 00000000 00000015 00000001 00000000 00000004 00000001"),
            "22P03"
        );
        // 短い
        assert_eq!(
            t("00000001 00000000 00000015 00000002 00000000 00000002 0001"),
            "08P01"
        );
        // 余り
        assert_eq!(t("00000000 00000000 00000015 00"), "22P03");
        assert_eq!(t("00000007 00000000 00000015"), "22P03");
    }

    #[test]
    fn unsupported_and_mismatch() {
        assert!(!supports_binary(1033));
        assert_eq!(
            binary_send(&Datum::Int4(1), SqlType::of(1033))
                .unwrap_err()
                .sqlstate
                .0,
            "42883"
        );
        assert!(binary_send(&Datum::Int4(1), SqlType::INT8).is_err());
    }

    #[test]
    fn roundtrip_boundaries() {
        let cases = [
            (oid::INT2, Datum::Int2(i16::MIN)),
            (oid::INT2, Datum::Int2(i16::MAX)),
            (oid::INT4, Datum::Int4(i32::MIN)),
            (oid::INT8, Datum::Int8(i64::MAX)),
            (oid::FLOAT4, Datum::Float4(-0.0)),
            (oid::FLOAT4, Datum::Float4(f32::INFINITY)),
            (oid::FLOAT8, Datum::Float8(-0.0)),
            (oid::FLOAT8, Datum::Float8(f64::NEG_INFINITY)),
            (oid::CHAR, Datum::Char(b'x')),
            (oid::OID, Datum::Oid(u32::MAX)),
            (oid::XID, Datum::Xid(7)),
            (oid::CID, Datum::Cid(7)),
            (oid::TEXT, Datum::Text("x".repeat(1 << 20))),
            (oid::VARCHAR, Datum::Text("é".into())),
            (oid::OIDVECTOR, Datum::OidVector(vec![1, 2, u32::MAX])),
            (oid::OIDVECTOR, Datum::OidVector(vec![])),
        ];
        for (o, d) in cases {
            let ty = SqlType::of(o);
            let b = binary_send(&d, ty).unwrap();
            let back = input_binary(&b, ty).unwrap();
            assert_eq!(binary_send(&back, ty).unwrap(), b, "{d:?}");
        }
    }

    proptest! {
        #[test]
        fn fuzz_no_panic(bytes in proptest::collection::vec(any::<u8>(), 0..64), idx in 0usize..24) {
            let oids = [
                oid::BOOL, oid::CHAR, oid::NAME, oid::INT8, oid::INT2, oid::INT4, oid::FLOAT4,
                oid::FLOAT8, oid::TEXT, oid::VARCHAR, oid::UNKNOWN, oid::PG_NODE_TREE, oid::OID,
                oid::XID, oid::CID, oid::REGPROC, oid::REGCLASS, oid::REGTYPE, oid::TID,
                oid::INT2VECTOR, oid::OIDVECTOR, oid::ACLITEM, oid::NUMERIC, oid::DATE,
            ];
            if let Err(e) = input_binary(&bytes, SqlType::of(oids[idx])) {
                prop_assert!(
                    ["08P01", "22P03", "22021", "42622", "42883", "0A000", "22003", "22008"]
                        .contains(&e.sqlstate.0)
                );
            }
        }

        #[test]
        fn int_roundtrip(v in any::<i64>(), w in any::<i32>(), x in any::<f64>()) {
            for (o, d) in [(oid::INT8, Datum::Int8(v)), (oid::INT4, Datum::Int4(w))] {
                let ty = SqlType::of(o);
                let b = binary_send(&d, ty).unwrap();
                prop_assert_eq!(input_binary(&b, ty).unwrap(), d);
            }
            let b = binary_send(&Datum::Float8(x), SqlType::FLOAT8).unwrap();
            let Datum::Float8(y) = input_binary(&b, SqlType::FLOAT8).unwrap() else { panic!() };
            prop_assert_eq!(x.to_bits(), y.to_bits());
        }
    }
}
