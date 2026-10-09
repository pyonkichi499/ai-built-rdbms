//! 配列（TY-5）。値の表現、ディスク形式（PostgreSQL の `ArrayType` と同じ）、比較・ハッシュ。
//! 入出力（`array_in` / `array_out`）とバイナリは TY-5 の後続で足す。

#![allow(
    clippy::cast_possible_truncation,
    clippy::cast_possible_wrap,
    clippy::cast_sign_loss
)]

use std::borrow::Cow;
use std::cmp::Ordering;
use std::hash::Hasher;

use super::datetime::{
    decode_date, decode_timestamp, decode_timestamptz, encode_date, encode_timestamp,
    encode_timestamptz,
};
use super::hash::hash_datum;
use super::numeric::{decode_numeric, encode_numeric};
use super::typeinfo::{ElemInfo, elem_info};
use super::{Datum, Oid, Tid, cmp_datum, interval, oid};
use crate::error::{Error, Result, sqlstate};

/// PostgreSQL の `MAXDIM`。
pub const MAXDIM: usize = 6;
/// M5 の入口が許す次元数（TY-D2）。
pub const M5_MAX_NDIM: usize = 1;
/// `MaxAllocSize / sizeof(Datum)`。
pub const MAX_ARRAY_SIZE: usize = 134_217_727;

/// 配列の 1 次元分（PostgreSQL の `ARR_DIMS` / `ARR_LBOUND`）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ArrayDim {
    pub len: i32,
    pub lbound: i32,
}

/// PostgreSQL の `ArrayType` と同じ意味の配列値。`ndim = dims.len()`（空の配列は `dims` が空）。
/// M5 が作るのは `dims.len() <= 1` だけ。`items` は行優先の平坦な並びで、NULL 要素は `Datum::Null`。
#[derive(Clone, Debug, PartialEq)]
pub struct ArrayValue {
    pub elem_type: Oid,
    pub dims: Vec<ArrayDim>,
    pub items: Vec<Datum>,
}

impl ArrayValue {
    /// 下限 1 の 1 次元配列。`items` が空なら空の配列（`dims` なし）。
    pub fn one_dim(elem_type: Oid, items: Vec<Datum>) -> Self {
        let dims = if items.is_empty() {
            Vec::new()
        } else {
            vec![ArrayDim {
                len: i32::try_from(items.len()).unwrap_or(i32::MAX),
                lbound: 1,
            }]
        };
        Self {
            elem_type,
            dims,
            items,
        }
    }

    /// `ndim = 0` の空の配列。
    pub fn empty(elem_type: Oid) -> Self {
        Self {
            elem_type,
            dims: Vec::new(),
            items: Vec::new(),
        }
    }

    /// 下限 `lbound` の 1 次元配列。件数が上限を超える、または上限の添字が `i32` を超えたら `54000`。
    pub fn from_vec(elem_type: Oid, lbound: i32, items: Vec<Datum>) -> Result<Self> {
        if items.is_empty() {
            return Ok(Self::empty(elem_type));
        }
        if items.len() > MAX_ARRAY_SIZE {
            return Err(Error::new(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                format!("array size exceeds the maximum allowed ({MAX_ARRAY_SIZE})"),
            ));
        }
        let len = i32::try_from(items.len()).map_err(|_| {
            Error::new(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "array size exceeds the maximum allowed",
            )
        })?;
        if lbound.checked_add(len - 1).is_none() {
            return Err(Error::new(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                format!("array lower bound is too large: {lbound}"),
            ));
        }
        Ok(Self {
            elem_type,
            dims: vec![ArrayDim { len, lbound }],
            items,
        })
    }

    pub fn ndim(&self) -> usize {
        self.dims.len()
    }

    pub fn nitems(&self) -> usize {
        self.items.len()
    }

    /// `dim` は 1 始まり。範囲外は `None`。
    pub fn lower(&self, dim: i32) -> Option<i32> {
        let d = self.dim_at(dim)?;
        Some(d.lbound)
    }

    pub fn upper(&self, dim: i32) -> Option<i32> {
        let d = self.dim_at(dim)?;
        d.lbound.checked_add(d.len - 1)
    }

    fn dim_at(&self, dim: i32) -> Option<&ArrayDim> {
        let i = usize::try_from(dim.checked_sub(1)?).ok()?;
        self.dims.get(i)
    }

    /// 1 次元の添字参照。範囲外（次元が 1 でない場合も）は `None`、NULL 要素は `Some(&Datum::Null)`。
    pub fn get1(&self, subscript: i32) -> Option<&Datum> {
        if self.dims.len() != 1 {
            return None;
        }
        let d = self.dims[0];
        let idx = i64::from(subscript) - i64::from(d.lbound);
        if idx < 0 || idx >= i64::from(d.len) {
            return None;
        }
        self.items.get(usize::try_from(idx).ok()?)
    }

    /// 不変条件の検査。違反は `Error::internal`。
    pub fn check(&self) -> Result<()> {
        let bad = |m: &str| Err(Error::internal(format!("invalid array value: {m}")));
        if self.dims.len() > MAXDIM {
            return bad("too many dimensions");
        }
        if self.dims.is_empty() {
            if !self.items.is_empty() {
                return bad("no dimensions but has items");
            }
            return Ok(());
        }
        let mut n: usize = 1;
        for d in &self.dims {
            if d.len < 1 {
                return bad("dimension length is less than 1");
            }
            if d.lbound.checked_add(d.len).is_none() {
                return bad("upper bound overflows");
            }
            n = n.saturating_mul(d.len as usize);
        }
        if n != self.items.len() || n > MAX_ARRAY_SIZE {
            return bad("item count does not match the dimensions");
        }
        if self.items.iter().any(|i| {
            matches!(
                i,
                Datum::Array(_) | Datum::Int2Vector(_) | Datum::OidVector(_)
            )
        }) {
            return bad("an array element is an array");
        }
        Ok(())
    }

    /// `int2[]`（下限 1）。
    pub fn from_int2s(v: &[i16]) -> Self {
        Self::one_dim(oid::INT2, v.iter().map(|x| Datum::Int2(*x)).collect())
    }

    pub fn to_int2s(&self) -> Result<Vec<i16>> {
        self.collect_items(oid::INT2, |d| match d {
            Datum::Int2(v) => Some(*v),
            _ => None,
        })
    }

    /// `oid[]`（下限 1）。
    pub fn from_oids(v: &[u32]) -> Self {
        Self::one_dim(oid::OID, v.iter().map(|x| Datum::Oid(*x)).collect())
    }

    pub fn to_oids(&self) -> Result<Vec<u32>> {
        self.collect_items(oid::OID, |d| match d {
            Datum::Oid(v) => Some(*v),
            _ => None,
        })
    }

    /// `text[]`（下限 1）。
    pub fn from_texts<S: AsRef<str>>(v: &[S]) -> Self {
        Self::one_dim(
            oid::TEXT,
            v.iter()
                .map(|s| Datum::Text(s.as_ref().to_owned()))
                .collect(),
        )
    }

    pub fn to_texts(&self) -> Result<Vec<String>> {
        self.collect_items(oid::TEXT, |d| match d {
            Datum::Text(s) => Some(s.clone()),
            _ => None,
        })
    }

    fn collect_items<T>(&self, elem: Oid, f: impl Fn(&Datum) -> Option<T>) -> Result<Vec<T>> {
        if self.elem_type != elem {
            return Err(Error::internal(format!(
                "array has element type {}, expected {elem}",
                self.elem_type
            )));
        }
        self.items
            .iter()
            .map(|d| f(d).ok_or_else(|| Error::internal("array has a NULL or mismatched element")))
            .collect()
    }
}

/// `anyarray` を取る関数の入口。`Array` はそのまま借り、`Int2Vector` と `OidVector` は下限 0 に読み替える。
pub fn array_view(d: &Datum) -> Option<Cow<'_, ArrayValue>> {
    fn vector(elem: Oid, items: Vec<Datum>) -> ArrayValue {
        let mut a = ArrayValue::one_dim(elem, items);
        for dim in &mut a.dims {
            dim.lbound = 0;
        }
        a
    }
    match d {
        Datum::Array(a) => Some(Cow::Borrowed(a)),
        Datum::Int2Vector(v) => Some(Cow::Owned(vector(
            oid::INT2,
            v.iter().map(|x| Datum::Int2(*x)).collect(),
        ))),
        Datum::OidVector(v) => Some(Cow::Owned(vector(
            oid::OID,
            v.iter().map(|x| Datum::Oid(*x)).collect(),
        ))),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// 比較・ハッシュ
// ---------------------------------------------------------------------------

/// PostgreSQL の `array_cmp` と同じ全順序。Equal は dims・lbound・要素がすべて同じ（NULL どうしは等しい）。
pub fn cmp_array(a: &ArrayValue, b: &ArrayValue) -> Ordering {
    for (x, y) in a.items.iter().zip(&b.items) {
        let c = match (x.is_null(), y.is_null()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Greater,
            (false, true) => Ordering::Less,
            (false, false) => cmp_datum(x, y),
        };
        if c != Ordering::Equal {
            return c;
        }
    }
    a.items
        .len()
        .cmp(&b.items.len())
        .then_with(|| a.dims.len().cmp(&b.dims.len()))
        .then_with(|| {
            let lens = |v: &ArrayValue| v.dims.iter().map(|d| d.len).collect::<Vec<_>>();
            let lbs = |v: &ArrayValue| v.dims.iter().map(|d| d.lbound).collect::<Vec<_>>();
            lens(a).cmp(&lens(b)).then_with(|| lbs(a).cmp(&lbs(b)))
        })
}

/// `cmp_array` の Equal と整合するハッシュ（`hash_datum` の `Datum::Array` と同じ混ぜ方の要素部分）。
pub fn hash_array(a: &ArrayValue, state: &mut dyn Hasher) {
    state.write_usize(a.dims.len());
    for d in &a.dims {
        state.write_i32(d.len);
        state.write_i32(d.lbound);
    }
    for item in &a.items {
        hash_datum(item, state);
    }
}

/// `=` の本体。要素型が違えば `42804`。
pub fn array_eq(a: &ArrayValue, b: &ArrayValue) -> Result<bool> {
    if a.elem_type != b.elem_type {
        return Err(Error::new(
            sqlstate::DATATYPE_MISMATCH,
            "cannot compare arrays of different element types",
        ));
    }
    Ok(cmp_array(a, b) == Ordering::Equal)
}

// ---------------------------------------------------------------------------
// ディスク形式（PostgreSQL の ArrayType。ペイロードは vl_len_ を除いた部分）
// ---------------------------------------------------------------------------

fn maxalign(n: usize) -> usize {
    (n + 7) & !7
}

fn align_to(n: usize, a: usize) -> usize {
    (n + a - 1) & !(a - 1)
}

fn corrupt(m: &str) -> Error {
    Error::corrupted(format!("corrupted array value in a tuple: {m}"))
}

fn mismatch(elem: Oid, d: &Datum) -> Error {
    Error::internal(format!(
        "array element {d:?} does not match element type {elem}"
    ))
}

/// 要素 1 個のペイロード（整列の詰め物は含まない。varlena は 4 バイトヘッダ付き）。
#[allow(clippy::match_same_arms)]
fn write_elem(info: ElemInfo, d: &Datum, out: &mut Vec<u8>) -> Result<()> {
    let e = info.oid;
    match (e, d) {
        (oid::BOOL, Datum::Bool(v)) => out.push(u8::from(*v)),
        (oid::CHAR, Datum::Char(v)) => out.push(*v),
        (oid::INT2, Datum::Int2(v)) => out.extend_from_slice(&v.to_le_bytes()),
        (oid::INT4, Datum::Int4(v)) => out.extend_from_slice(&v.to_le_bytes()),
        (oid::INT8, Datum::Int8(v)) => out.extend_from_slice(&v.to_le_bytes()),
        (oid::FLOAT4, Datum::Float4(v)) => out.extend_from_slice(&v.to_bits().to_le_bytes()),
        (oid::FLOAT8, Datum::Float8(v)) => out.extend_from_slice(&v.to_bits().to_le_bytes()),
        (oid::OID | oid::REGPROC | oid::REGCLASS | oid::REGTYPE, Datum::Oid(v))
        | (oid::XID, Datum::Xid(v))
        | (oid::CID, Datum::Cid(v)) => out.extend_from_slice(&v.to_le_bytes()),
        (oid::TID, Datum::Tid(t)) => {
            out.extend_from_slice(&t.block.to_le_bytes());
            out.extend_from_slice(&t.offset.to_le_bytes());
        }
        (oid::DATE, Datum::Date(v)) => out.extend_from_slice(&encode_date(*v)),
        (oid::TIMESTAMP, Datum::Timestamp(v)) => out.extend_from_slice(&encode_timestamp(*v)),
        (oid::TIMESTAMPTZ, Datum::TimestampTz(v)) => {
            out.extend_from_slice(&encode_timestamptz(*v));
        }
        (oid::TIME, Datum::Time(v)) => out.extend_from_slice(&v.to_le_bytes()),
        (oid::INTERVAL, Datum::Interval(v)) => out.extend_from_slice(&interval::encode(v)),
        (oid::UUID, Datum::Uuid(v)) => out.extend_from_slice(v),
        (oid::NAME, Datum::Text(s)) => {
            if s.len() > 63 {
                return Err(Error::internal(format!(
                    "name value is {} bytes; at most 63 are allowed",
                    s.len()
                )));
            }
            let start = out.len();
            out.resize(start + 64, 0);
            out[start..start + s.len()].copy_from_slice(s.as_bytes());
        }
        (oid::TEXT | oid::VARCHAR, Datum::Text(s)) | (oid::BPCHAR, Datum::BpChar(s)) => {
            write_var4(out, s.as_bytes());
        }
        (oid::BYTEA, Datum::Bytea(b)) => write_var4(out, b),
        (oid::NUMERIC, Datum::Numeric(n)) => {
            let mut p = Vec::new();
            encode_numeric(n, &mut p);
            write_var4(out, &p);
        }
        _ => return Err(mismatch(e, d)),
    }
    Ok(())
}

fn write_var4(out: &mut Vec<u8>, data: &[u8]) {
    let hdr = ((data.len() + 4) as u32) << 2;
    out.extend_from_slice(&hdr.to_le_bytes());
    out.extend_from_slice(data);
}

/// ペイロードを返す。長さの上限は呼び出し側（`54000`）。
pub fn encode_array(a: &ArrayValue) -> Result<Vec<u8>> {
    a.check()?;
    let info = elem_info(a.elem_type)?;
    let ndim = a.dims.len();
    let n = a.items.len();
    let has_null = a.items.iter().any(Datum::is_null);
    let bitmap = if has_null { n.div_ceil(8) } else { 0 };
    let overhead = maxalign(16 + 8 * ndim + bitmap);
    let mut buf = vec![0u8; overhead - 4];
    buf[0..4].copy_from_slice(&(ndim as i32).to_le_bytes());
    let dataoffset = if has_null { overhead as i32 } else { 0 };
    buf[4..8].copy_from_slice(&dataoffset.to_le_bytes());
    buf[8..12].copy_from_slice(&a.elem_type.to_le_bytes());
    for (i, d) in a.dims.iter().enumerate() {
        buf[12 + 4 * i..16 + 4 * i].copy_from_slice(&d.len.to_le_bytes());
        let lb = 12 + 4 * ndim + 4 * i;
        buf[lb..lb + 4].copy_from_slice(&d.lbound.to_le_bytes());
    }
    if has_null {
        let base = 12 + 8 * ndim;
        for (i, item) in a.items.iter().enumerate() {
            if !item.is_null() {
                buf[base + i / 8] |= 1 << (i % 8);
            }
        }
    }
    let align = usize::from(info.align);
    for item in &a.items {
        if item.is_null() {
            continue;
        }
        // 要素の整列はイメージ（4 バイトのヘッダ付き）の位置で数える。
        let pos = align_to(buf.len() + 4, align) - 4;
        buf.resize(pos, 0);
        write_elem(info, item, &mut buf)?;
        let end = align_to(buf.len() + 4, align) - 4;
        buf.resize(end, 0);
    }
    Ok(buf)
}

fn read_i32(b: &[u8], o: usize) -> i32 {
    i32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// 要素 1 個を `buf[*pos..]`（イメージ位置は `*pos + 4`）から読む。
fn read_elem(info: ElemInfo, buf: &[u8], pos: &mut usize) -> Result<Datum> {
    let align = usize::from(info.align);
    *pos = align_to(*pos + 4, align) - 4;
    let take = |pos: &mut usize, n: usize| -> Result<&[u8]> {
        let end = pos
            .checked_add(n)
            .filter(|e| *e <= buf.len())
            .ok_or_else(|| corrupt("element extends past the end"))?;
        let s = &buf[*pos..end];
        *pos = end;
        Ok(s)
    };
    let e = info.oid;
    Ok(match e {
        oid::BOOL => Datum::Bool(take(pos, 1)?[0] != 0),
        oid::CHAR => Datum::Char(take(pos, 1)?[0]),
        oid::INT2 => Datum::Int2(i16::from_le_bytes(
            take(pos, 2)?.try_into().unwrap_or_default(),
        )),
        oid::INT4 => Datum::Int4(i32::from_le_bytes(
            take(pos, 4)?.try_into().unwrap_or_default(),
        )),
        oid::INT8 => Datum::Int8(i64::from_le_bytes(
            take(pos, 8)?.try_into().unwrap_or_default(),
        )),
        oid::FLOAT4 => Datum::Float4(f32::from_bits(u32::from_le_bytes(
            take(pos, 4)?.try_into().unwrap_or_default(),
        ))),
        oid::FLOAT8 => Datum::Float8(f64::from_bits(u64::from_le_bytes(
            take(pos, 8)?.try_into().unwrap_or_default(),
        ))),
        oid::OID | oid::REGPROC | oid::REGCLASS | oid::REGTYPE => Datum::Oid(u32::from_le_bytes(
            take(pos, 4)?.try_into().unwrap_or_default(),
        )),
        oid::XID => Datum::Xid(u32::from_le_bytes(
            take(pos, 4)?.try_into().unwrap_or_default(),
        )),
        oid::CID => Datum::Cid(u32::from_le_bytes(
            take(pos, 4)?.try_into().unwrap_or_default(),
        )),
        oid::TID => {
            let b = take(pos, 6)?;
            Datum::Tid(Tid {
                block: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                offset: u16::from_le_bytes([b[4], b[5]]),
            })
        }
        oid::DATE => Datum::Date(decode_date(take(pos, 4)?)?),
        oid::TIMESTAMP => Datum::Timestamp(decode_timestamp(take(pos, 8)?)?),
        oid::TIMESTAMPTZ => Datum::TimestampTz(decode_timestamptz(take(pos, 8)?)?),
        oid::TIME => Datum::Time(i64::from_le_bytes(
            take(pos, 8)?.try_into().unwrap_or_default(),
        )),
        oid::INTERVAL => Datum::Interval(interval::decode_slice(take(pos, 16)?)?),
        oid::UUID => {
            let mut u = [0u8; 16];
            u.copy_from_slice(take(pos, 16)?);
            Datum::Uuid(u)
        }
        oid::NAME => {
            let b = take(pos, 64)?;
            let end = b.iter().position(|&c| c == 0).unwrap_or(64);
            Datum::Text(
                String::from_utf8(b[..end].to_vec()).map_err(|_| corrupt("name is not UTF-8"))?,
            )
        }
        oid::TEXT | oid::VARCHAR | oid::BPCHAR | oid::BYTEA | oid::NUMERIC => {
            let body = read_varlena(buf, pos)?;
            match e {
                oid::BYTEA => Datum::Bytea(body.to_vec()),
                oid::NUMERIC => Datum::Numeric(decode_numeric(body)?),
                _ => {
                    let s = String::from_utf8(body.to_vec())
                        .map_err(|_| corrupt("text is not valid UTF-8"))?;
                    if e == oid::BPCHAR {
                        Datum::BpChar(s)
                    } else {
                        Datum::Text(s)
                    }
                }
            }
        }
        _ => return Err(corrupt("unsupported element type")),
    })
}

/// 1 バイトヘッダと 4 バイトヘッダを受ける（圧縮・外部は不可）。
fn read_varlena<'a>(buf: &'a [u8], pos: &mut usize) -> Result<&'a [u8]> {
    let b = *buf
        .get(*pos)
        .ok_or_else(|| corrupt("varlena header past the end"))?;
    let (hdr, total) = if b & 1 == 1 {
        if b == 1 {
            return Err(corrupt("external varlena in an array"));
        }
        (1usize, usize::from(b >> 1))
    } else if b.trailing_zeros() >= 2 {
        let h = buf
            .get(*pos..*pos + 4)
            .ok_or_else(|| corrupt("varlena header past the end"))?;
        (
            4usize,
            (u32::from_le_bytes([h[0], h[1], h[2], h[3]]) >> 2) as usize,
        )
    } else {
        return Err(corrupt("compressed varlena in an array"));
    };
    if total < hdr {
        return Err(corrupt("bad varlena length"));
    }
    let end = pos
        .checked_add(total)
        .filter(|e| *e <= buf.len())
        .ok_or_else(|| corrupt("varlena extends past the end"))?;
    let body = &buf[*pos + hdr..end];
    *pos = end;
    Ok(body)
}

/// 失敗はすべて `XX001`。
pub fn decode_array(payload: &[u8], expected_elem: Oid) -> Result<ArrayValue> {
    if payload.len() < 12 {
        return Err(corrupt("too short"));
    }
    let ndim = read_i32(payload, 0);
    if !(0..=MAXDIM as i32).contains(&ndim) {
        return Err(corrupt("bad number of dimensions"));
    }
    let ndim = ndim as usize;
    let dataoffset = read_i32(payload, 4);
    let elemtype = u32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]);
    if elemtype != expected_elem {
        return Err(corrupt("element type does not match the column"));
    }
    let info = elem_info(elemtype).map_err(|_| corrupt("unsupported element type"))?;
    if payload.len() < 12 + 8 * ndim {
        return Err(corrupt("dimensions extend past the end"));
    }
    let mut dims = Vec::with_capacity(ndim);
    let mut n: usize = 1;
    for i in 0..ndim {
        let len = read_i32(payload, 12 + 4 * i);
        let lbound = read_i32(payload, 12 + 4 * ndim + 4 * i);
        if len < 0 || lbound.checked_add(len).is_none() {
            return Err(corrupt("bad dimension"));
        }
        n = n.saturating_mul(len as usize);
        if n > MAX_ARRAY_SIZE {
            return Err(corrupt("too many elements"));
        }
        dims.push(ArrayDim { len, lbound });
    }
    if ndim == 0 || n == 0 {
        return Ok(ArrayValue::empty(elemtype));
    }
    let has_null = dataoffset != 0;
    let bitmap = if has_null { n.div_ceil(8) } else { 0 };
    let overhead = maxalign(16 + 8 * ndim + bitmap);
    let data_start = if has_null {
        let off = usize::try_from(dataoffset).map_err(|_| corrupt("bad dataoffset"))?;
        if off < overhead || off > payload.len() + 4 || off % 8 != 0 {
            return Err(corrupt("bad dataoffset"));
        }
        off - 4
    } else {
        overhead - 4
    };
    if has_null && payload.len() < 12 + 8 * ndim + bitmap {
        return Err(corrupt("null bitmap extends past the end"));
    }
    if data_start > payload.len() {
        return Err(corrupt("data starts past the end"));
    }
    let mut items = Vec::with_capacity(n);
    let mut pos = data_start;
    let bm = 12 + 8 * ndim;
    for i in 0..n {
        if has_null && payload[bm + i / 8] & (1 << (i % 8)) == 0 {
            items.push(Datum::Null);
        } else {
            items.push(read_elem(info, payload, &mut pos)?);
        }
    }
    // 末尾は詰め物の分だけ短くてよい。それ以外の余りは壊れている。
    let end = align_to(pos + 4, 8) - 4;
    if pos > payload.len() || payload.len() > end.max(pos) {
        return Err(corrupt("trailing bytes after the last element"));
    }
    Ok(ArrayValue {
        elem_type: elemtype,
        dims,
        items,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::hash::DefaultHasher;

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    #[test]
    fn one_dim_shapes() {
        let a = ArrayValue::one_dim(23, vec![Datum::Int4(1), Datum::Null]);
        assert_eq!(a.ndim(), 1);
        assert_eq!(a.dims[0], ArrayDim { len: 2, lbound: 1 });
        let e = ArrayValue::one_dim(23, vec![]);
        assert_eq!(e.ndim(), 0);
    }

    #[test]
    fn pg_bytes_int4_with_null() {
        let a = ArrayValue::one_dim(23, vec![Datum::Int4(1), Datum::Null, Datum::Int4(3)]);
        let want =
            hex("01000000 20000000 17000000 03000000 01000000 05 00000000000000 01000000 03000000");
        assert_eq!(want.len(), 36);
        let got = encode_array(&a).unwrap();
        assert_eq!(got, want);
        assert_eq!(decode_array(&got, 23).unwrap(), a);
    }

    #[test]
    fn pg_bytes_text() {
        let a = ArrayValue::from_texts(&["a", "bb"]);
        let want = hex(
            "01000000 00000000 19000000 02000000 01000000 14000000 61 000000 18000000 6262 0000",
        );
        let got = encode_array(&a).unwrap();
        assert_eq!(got, want);
        assert_eq!(decode_array(&got, 25).unwrap(), a);
    }

    #[test]
    fn pg_bytes_empty_and_vector() {
        let e = ArrayValue::empty(23);
        let got = encode_array(&e).unwrap();
        assert_eq!(got, hex("00000000 00000000 17000000"));
        assert_eq!(decode_array(&got, 23).unwrap(), e);

        let v = array_view(&Datum::Int2Vector(vec![1, 2]))
            .unwrap()
            .into_owned();
        let got = encode_array(&v).unwrap();
        assert_eq!(
            got,
            hex("01000000 00000000 15000000 02000000 00000000 0100 0200")
        );
        assert_eq!(decode_array(&got, 21).unwrap(), v);
    }

    #[test]
    fn one_byte_header_elements_are_accepted() {
        // text[] {a} で要素が 1 バイトヘッダ（0x05 = 長さ 2）のもの。
        let p = hex("01000000 00000000 19000000 01000000 01000000 05 61");
        let a = decode_array(&p, 25).unwrap();
        assert_eq!(a, ArrayValue::from_texts(&["a"]));
    }

    #[test]
    fn roundtrip_lengths_around_varlena_thresholds() {
        for n in [
            1usize, 2, 30, 31, 32, 33, 60, 61, 62, 63, 64, 65, 120, 126, 127, 128, 129, 190, 191,
            192, 193,
        ] {
            let ints = ArrayValue::from_int2s(&(0..n as i16).collect::<Vec<_>>());
            let p = encode_array(&ints).unwrap();
            assert_eq!(decode_array(&p, 21).unwrap(), ints, "int2 n={n}");

            let mut items: Vec<Datum> = (0..n as i32).map(Datum::Int4).collect();
            if n > 2 {
                items[1] = Datum::Null;
            }
            let a = ArrayValue::one_dim(23, items);
            let p = encode_array(&a).unwrap();
            assert_eq!(decode_array(&p, 23).unwrap(), a, "int4 n={n}");

            let oids = ArrayValue::from_oids(&(0..n as u32).collect::<Vec<_>>());
            let p = encode_array(&oids).unwrap();
            assert_eq!(decode_array(&p, 26).unwrap(), oids, "oid n={n}");

            let texts: Vec<String> = (0..n).map(|i| "x".repeat(i % 7)).collect();
            let t = ArrayValue::from_texts(&texts);
            let p = encode_array(&t).unwrap();
            assert_eq!(decode_array(&p, 25).unwrap(), t, "text n={n}");
            assert_eq!(t.to_texts().unwrap(), texts);

            // ペイロード全体の長さを 64・128・192 付近に合わせる検査（バイト単位の arrays）
            let bytes =
                ArrayValue::one_dim(oid::BYTEA, vec![Datum::Bytea(vec![7u8; n]), Datum::Null]);
            let p = encode_array(&bytes).unwrap();
            assert_eq!(decode_array(&p, oid::BYTEA).unwrap(), bytes, "bytea n={n}");
        }
    }

    #[test]
    fn roundtrip_other_element_types() {
        let cases = vec![
            ArrayValue::one_dim(
                oid::BOOL,
                vec![Datum::Bool(true), Datum::Null, Datum::Bool(false)],
            ),
            ArrayValue::one_dim(oid::CHAR, vec![Datum::Char(b'x')]),
            ArrayValue::one_dim(oid::INT8, vec![Datum::Int8(-5), Datum::Int8(i64::MAX)]),
            ArrayValue::one_dim(oid::FLOAT4, vec![Datum::Float4(1.5)]),
            ArrayValue::one_dim(oid::FLOAT8, vec![Datum::Float8(-0.25), Datum::Null]),
            ArrayValue::one_dim(oid::NAME, vec![Datum::Text("abc".into())]),
            ArrayValue::one_dim(oid::UUID, vec![Datum::Uuid([9; 16])]),
            ArrayValue::one_dim(
                oid::TID,
                vec![Datum::Tid(Tid {
                    block: 3,
                    offset: 4,
                })],
            ),
            ArrayValue::one_dim(oid::BPCHAR, vec![Datum::BpChar("ab ".into())]),
        ];
        for a in cases {
            let p = encode_array(&a).unwrap();
            assert_eq!(decode_array(&p, a.elem_type).unwrap(), a);
        }
        let neg = ArrayValue::from_vec(23, -3, vec![Datum::Int4(1), Datum::Int4(2)]).unwrap();
        let p = encode_array(&neg).unwrap();
        assert_eq!(decode_array(&p, 23).unwrap(), neg);
    }

    #[test]
    fn corrupt_payloads_are_xx001() {
        let a = ArrayValue::from_int2s(&[1, 2, 3]);
        let p = encode_array(&a).unwrap();
        assert!(decode_array(&p, 23).is_err());
        for cut in [0usize, 5, 11, 15, p.len() - 4] {
            let e = decode_array(&p[..cut], 21).unwrap_err();
            assert_eq!(e.sqlstate.code(), "XX001", "cut={cut}");
        }
        let mut bad = p.clone();
        bad[0] = 9;
        assert_eq!(decode_array(&bad, 21).unwrap_err().sqlstate.code(), "XX001");
        let mut long = p.clone();
        long.extend_from_slice(&[0; 16]);
        assert_eq!(
            decode_array(&long, 21).unwrap_err().sqlstate.code(),
            "XX001"
        );
    }

    #[test]
    fn accessors_and_conversions() {
        let a = ArrayValue::from_vec(23, 0, vec![Datum::Int4(5), Datum::Null]).unwrap();
        assert_eq!((a.lower(1), a.upper(1)), (Some(0), Some(1)));
        assert_eq!(a.lower(2), None);
        assert_eq!(a.get1(0), Some(&Datum::Int4(5)));
        assert_eq!(a.get1(1), Some(&Datum::Null));
        assert_eq!(a.get1(2), None);
        assert!(a.check().is_ok());
        assert_eq!(
            ArrayValue::from_int2s(&[1, 2]).to_int2s().unwrap(),
            vec![1, 2]
        );
        assert!(a.to_int2s().is_err());
        assert_eq!(ArrayValue::from_oids(&[7]).to_oids().unwrap(), vec![7]);
        assert!(ArrayValue::from_vec(23, i32::MAX, vec![Datum::Int4(1), Datum::Int4(2)]).is_err());
    }

    #[test]
    fn cmp_hash_eq() {
        let low = ArrayValue::from_int2s(&[1, 2]);
        let high = ArrayValue::from_int2s(&[1, 3]);
        let with_null = ArrayValue::one_dim(21, vec![Datum::Int2(1), Datum::Null]);
        assert_eq!(cmp_array(&low, &low), Ordering::Equal);
        assert_eq!(cmp_array(&low, &high), Ordering::Less);
        assert_eq!(cmp_array(&with_null, &high), Ordering::Greater);
        let mut shifted = low.clone();
        shifted.dims[0].lbound = 0;
        assert_ne!(cmp_array(&low, &shifted), Ordering::Equal);
        let hash_of = |arr: &ArrayValue| {
            let mut hasher = DefaultHasher::new();
            hash_array(arr, &mut hasher);
            hasher.finish()
        };
        assert_eq!(hash_of(&low), hash_of(&low.clone()));
        assert_ne!(hash_of(&low), hash_of(&shifted));
        assert!(array_eq(&low, &low).unwrap());
        let texts = ArrayValue::from_texts(&["a"]);
        assert_eq!(array_eq(&low, &texts).unwrap_err().sqlstate.code(), "42804");
    }
}
