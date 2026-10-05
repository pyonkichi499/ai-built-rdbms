//! Tuple format (35-byte header, null bitmap, column encoding)
//! (`m2.md` §3.5, §3.6, §6.5.1).

#![allow(clippy::cast_possible_truncation, clippy::cast_lossless)]

use crate::error::{Error, Result, sqlstate};
use crate::storage::{
    MAX_HEAP_ATTRIBUTE_NUMBER, MAX_HEAP_TUPLE_SIZE, SIZE_OF_HEAP_TUPLE_HEADER, TupleDesc, WriteCtx,
};
use crate::txn::{CommandId, Xid};
use crate::types::{Datum, Row, Tid, oid};

pub const HEAP_HASNULL: u16 = 0x0001;
pub const HEAP_HASVARWIDTH: u16 = 0x0002;
pub const HEAP_HASEXTERNAL: u16 = 0x0004;
pub const HEAP_XMAX_KEYSHR_LOCK: u16 = 0x0010;
pub const HEAP_COMBOCID: u16 = 0x0020;
pub const HEAP_XMAX_EXCL_LOCK: u16 = 0x0040;
pub const HEAP_XMAX_LOCK_ONLY: u16 = 0x0080;
pub const HEAP_XMIN_COMMITTED: u16 = 0x0100;
pub const HEAP_XMIN_INVALID: u16 = 0x0200;
pub const HEAP_XMAX_COMMITTED: u16 = 0x0400;
pub const HEAP_XMAX_INVALID: u16 = 0x0800;
pub const HEAP_XMAX_IS_MULTI: u16 = 0x1000;
pub const HEAP_UPDATED: u16 = 0x2000;
/// Bits that must never be set in `t_infomask`.
const INFOMASK_FORBIDDEN: u16 = HEAP_HASEXTERNAL | HEAP_COMBOCID | 0x0008 | 0x4000 | 0x8000;

pub const HEAP_NATTS_MASK: u16 = 0x07FF;
pub const HEAP_KEYS_UPDATED: u16 = 0x2000;
const INFOMASK2_FORBIDDEN: u16 = 0x1800;

/// Every lock-related xmax bit cleared when a tuple is deleted.
pub const HEAP_XMAX_LOCK_BITS: u16 = HEAP_XMAX_KEYSHR_LOCK
    | HEAP_XMAX_EXCL_LOCK
    | HEAP_XMAX_LOCK_ONLY
    | HEAP_XMAX_IS_MULTI
    | HEAP_XMAX_INVALID
    | HEAP_XMAX_COMMITTED;

/// Flags for [`form_tuple`].
#[derive(Clone, Copy, Debug, Default)]
pub struct TupleFlags {
    /// Set `HEAP_UPDATED`.
    pub updated: bool,
}

fn maxalign(n: usize) -> usize {
    (n + 7) & !7
}

fn align_up(n: usize, a: usize) -> usize {
    (n + a - 1) & !(a - 1)
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Bool,
    Char,
    Int2,
    Int4,
    Int8,
    Float4,
    Float8,
    Oid,
    Xid,
    Cid,
    Tid,
    Name,
    Text,
    OidVector,
    /// Types that exist only for always-NULL columns.
    NullOnly,
}

fn kind_of(type_oid: u32) -> Result<Kind> {
    Ok(match type_oid {
        oid::BOOL => Kind::Bool,
        oid::CHAR => Kind::Char,
        oid::INT2 => Kind::Int2,
        oid::INT4 => Kind::Int4,
        oid::INT8 => Kind::Int8,
        oid::FLOAT4 => Kind::Float4,
        oid::FLOAT8 => Kind::Float8,
        oid::OID | oid::REGPROC => Kind::Oid,
        oid::XID => Kind::Xid,
        oid::CID => Kind::Cid,
        oid::TID => Kind::Tid,
        oid::NAME => Kind::Name,
        oid::TEXT | oid::VARCHAR | oid::PG_NODE_TREE | oid::UNKNOWN => Kind::Text,
        oid::OIDVECTOR => Kind::OidVector,
        oid::ACLITEM
        | oid::TIMESTAMPTZ
        | oid::ANYARRAY
        | oid::ACLITEM_ARRAY
        | oid::TEXT_ARRAY
        | oid::INT2_ARRAY
        | oid::OID_ARRAY
        | oid::CHAR_ARRAY => Kind::NullOnly,
        other => {
            return Err(Error::not_supported(format!(
                "type with OID {other} cannot be stored in a heap tuple yet"
            )));
        }
    })
}

fn type_mismatch(type_oid: u32, d: &Datum) -> Error {
    Error::internal(format!(
        "datum {d:?} does not match column type with OID {type_oid}"
    ))
}

fn bitmap_len(natts: usize) -> usize {
    natts.div_ceil(8)
}

#[allow(clippy::too_many_lines)]
/// Encodes a row. Fails with `54000` if the row is too big, `0A000` for a
/// NULL-only type with a value.
pub fn form_tuple(
    desc: &TupleDesc,
    row: &[Datum],
    w: &WriteCtx,
    flags: TupleFlags,
) -> Result<Vec<u8>> {
    match form_tuple_with(desc, row, w, flags, false) {
        Err(e) if e.sqlstate == sqlstate::PROGRAM_LIMIT_EXCEEDED => {
            form_tuple_with(desc, row, w, flags, true)
        }
        r => r,
    }
}

#[allow(clippy::too_many_lines)]
fn form_tuple_with(
    desc: &TupleDesc,
    row: &[Datum],
    w: &WriteCtx,
    flags: TupleFlags,
    compress: bool,
) -> Result<Vec<u8>> {
    let natts = desc.attrs.len();
    if row.len() != natts {
        return Err(Error::internal(format!(
            "row has {} values but the tuple descriptor has {natts} columns",
            row.len()
        )));
    }
    if natts > MAX_HEAP_ATTRIBUTE_NUMBER {
        return Err(Error::new(
            sqlstate::TOO_MANY_COLUMNS,
            "tables can have at most 1600 columns",
        ));
    }
    let has_null = row.iter().any(Datum::is_null);
    let hoff = maxalign(SIZE_OF_HEAP_TUPLE_HEADER + if has_null { bitmap_len(natts) } else { 0 });
    let mut buf = vec![0u8; hoff];
    let mut has_var = false;
    let mut bits = vec![0u8; bitmap_len(natts)];

    for (i, (attr, d)) in desc.attrs.iter().zip(row).enumerate() {
        if d.is_null() {
            continue;
        }
        bits[i / 8] |= 1 << (i % 8);
        let kind = kind_of(attr.type_oid)?;
        let align = attr.align as usize;
        let pad_to = |buf: &mut Vec<u8>, a: usize| {
            let n = align_up(buf.len(), a);
            buf.resize(n, 0);
        };
        match (kind, d) {
            (Kind::NullOnly, _) => {
                return Err(Error::not_supported(format!(
                    "type with OID {} is not supported yet",
                    attr.type_oid
                )));
            }
            (Kind::Bool, Datum::Bool(v)) => buf.push(u8::from(*v)),
            (Kind::Char, Datum::Char(v)) => buf.push(*v),
            (Kind::Int2, Datum::Int2(v)) => {
                pad_to(&mut buf, align);
                buf.extend_from_slice(&v.to_le_bytes());
            }
            (Kind::Int4, Datum::Int4(v)) => {
                pad_to(&mut buf, align);
                buf.extend_from_slice(&v.to_le_bytes());
            }
            (Kind::Int8, Datum::Int8(v)) => {
                pad_to(&mut buf, align);
                buf.extend_from_slice(&v.to_le_bytes());
            }
            (Kind::Float4, Datum::Float4(v)) => {
                pad_to(&mut buf, align);
                buf.extend_from_slice(&v.to_bits().to_le_bytes());
            }
            (Kind::Float8, Datum::Float8(v)) => {
                pad_to(&mut buf, align);
                buf.extend_from_slice(&v.to_bits().to_le_bytes());
            }
            (Kind::Oid, Datum::Oid(v))
            | (Kind::Xid, Datum::Xid(v))
            | (Kind::Cid, Datum::Cid(v)) => {
                pad_to(&mut buf, align);
                buf.extend_from_slice(&v.to_le_bytes());
            }
            (Kind::Tid, Datum::Tid(t)) => {
                pad_to(&mut buf, align);
                buf.extend_from_slice(&t.block.to_le_bytes());
                buf.extend_from_slice(&t.offset.to_le_bytes());
            }
            (Kind::Name, Datum::Text(s)) => {
                if s.len() > 63 {
                    return Err(Error::internal(format!(
                        "name value is {} bytes; at most 63 are allowed",
                        s.len()
                    )));
                }
                let start = buf.len();
                buf.resize(start + 64, 0);
                buf[start..start + s.len()].copy_from_slice(s.as_bytes());
            }
            (Kind::Text, Datum::Text(s)) => {
                has_var = true;
                write_varlena(&mut buf, s.as_bytes(), align, compress);
            }
            (Kind::OidVector, Datum::OidVector(v)) => {
                has_var = true;
                let mut data = Vec::with_capacity(v.len() * 4);
                for x in v {
                    data.extend_from_slice(&x.to_le_bytes());
                }
                write_varlena(&mut buf, &data, align, compress);
            }
            _ => return Err(type_mismatch(attr.type_oid, d)),
        }
        if buf.len() > 4 * MAX_HEAP_TUPLE_SIZE {
            // Stop early on absurdly large rows.
            break;
        }
    }

    if buf.len() > MAX_HEAP_TUPLE_SIZE {
        return Err(Error::new(
            sqlstate::PROGRAM_LIMIT_EXCEEDED,
            format!(
                "row is too big: size {}, maximum size {MAX_HEAP_TUPLE_SIZE}",
                buf.len()
            ),
        ));
    }

    let mut infomask = HEAP_XMAX_INVALID;
    if has_null {
        infomask |= HEAP_HASNULL;
    }
    if has_var {
        infomask |= HEAP_HASVARWIDTH;
    }
    if flags.updated {
        infomask |= HEAP_UPDATED;
    }
    let hdr = TupleHeader {
        xmin: w.xid,
        xmax: Xid::INVALID,
        cmin: w.cid,
        cmax: 0,
        ctid: Tid {
            block: 0,
            offset: 0,
        },
        infomask2: natts as u16 & HEAP_NATTS_MASK,
        infomask,
        hoff: hoff as u8,
    };
    hdr.write(&mut buf);
    if has_null {
        buf[SIZE_OF_HEAP_TUPLE_HEADER..SIZE_OF_HEAP_TUPLE_HEADER + bits.len()]
            .copy_from_slice(&bits);
    }
    Ok(buf)
}

fn write_varlena(buf: &mut Vec<u8>, data: &[u8], align: usize, compress: bool) {
    if compress
        && data.len() >= 256
        && let Some(c) = lz_compress(data)
    {
        let n = align_up(buf.len(), align);
        buf.resize(n, 0);
        let hdr = (((c.len() + 8) as u32) << 2) | 2;
        buf.extend_from_slice(&hdr.to_le_bytes());
        buf.extend_from_slice(&(data.len() as u32).to_le_bytes());
        buf.extend_from_slice(&c);
        return;
    }
    if data.len() < 127 {
        buf.push((((data.len() + 1) << 1) | 1) as u8);
    } else {
        let n = align_up(buf.len(), align);
        buf.resize(n, 0);
        let hdr = ((data.len() + 4) as u32) << 2;
        buf.extend_from_slice(&hdr.to_le_bytes());
    }
    buf.extend_from_slice(data);
}

const LZ_MIN_MATCH: usize = 4;
const LZ_MAX_MATCH: usize = 0x7F + LZ_MIN_MATCH;
const LZ_MAX_OFFSET: usize = 0xFFFF;
const LZ_HASH_BITS: u32 = 13;

fn lz_hash(b: &[u8]) -> usize {
    let v = u32::from_le_bytes([b[0], b[1], b[2], b[3]]);
    (v.wrapping_mul(2_654_435_761) >> (32 - LZ_HASH_BITS)) as usize
}

fn lz_flush_literals(out: &mut Vec<u8>, mut lit: &[u8]) {
    while !lit.is_empty() {
        let n = lit.len().min(0x80);
        out.push((n - 1) as u8);
        out.extend_from_slice(&lit[..n]);
        lit = &lit[n..];
    }
}

/// Inline compression of a varlena payload (a small LZ77: a tag below 0x80
/// is a literal run of `tag + 1` bytes; a tag from 0x80 is a copy of
/// `(tag & 0x7F) + 4` bytes from a little-endian u16 offset back).
/// Returns `None` unless it saves at least a quarter.
fn lz_compress(data: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut table = vec![usize::MAX; 1 << LZ_HASH_BITS];
    let (mut i, mut lit_start) = (0, 0);
    while i + LZ_MIN_MATCH <= data.len() {
        let h = lz_hash(&data[i..]);
        let cand = table[h];
        table[h] = i;
        if cand != usize::MAX && i - cand <= LZ_MAX_OFFSET && data[cand..cand + 4] == data[i..i + 4]
        {
            let mut len = LZ_MIN_MATCH;
            while len < LZ_MAX_MATCH && i + len < data.len() && data[cand + len] == data[i + len] {
                len += 1;
            }
            lz_flush_literals(&mut out, &data[lit_start..i]);
            out.push(0x80 | (len - LZ_MIN_MATCH) as u8);
            out.extend_from_slice(&((i - cand) as u16).to_le_bytes());
            i += len;
            lit_start = i;
        } else {
            i += 1;
        }
    }
    lz_flush_literals(&mut out, &data[lit_start..]);
    (out.len() * 4 <= data.len() * 3).then_some(out)
}

fn lz_decompress(src: &[u8], raw_len: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(raw_len.min(1 << 24));
    let mut i = 0;
    while i < src.len() {
        let tag = src[i];
        i += 1;
        if tag < 0x80 {
            let n = usize::from(tag) + 1;
            let lit = src
                .get(i..i + n)
                .ok_or_else(|| corrupt("compressed value is truncated"))?;
            out.extend_from_slice(lit);
            i += n;
        } else {
            let off = src
                .get(i..i + 2)
                .map(|b| usize::from(u16::from_le_bytes([b[0], b[1]])))
                .ok_or_else(|| corrupt("compressed value is truncated"))?;
            i += 2;
            if off == 0 || off > out.len() {
                return Err(corrupt("compressed value has a bad back reference"));
            }
            let len = usize::from(tag & 0x7F) + LZ_MIN_MATCH;
            for _ in 0..len {
                out.push(out[out.len() - off]);
            }
        }
        if out.len() > raw_len {
            return Err(corrupt("compressed value is longer than declared"));
        }
    }
    if out.len() != raw_len {
        return Err(corrupt("compressed value has the wrong length"));
    }
    Ok(out)
}

fn corrupt(msg: impl Into<String>) -> Error {
    Error::corrupted(format!("invalid heap tuple: {}", msg.into()))
}

/// Cursor over the column area of a tuple.
struct Reader<'a> {
    bytes: &'a [u8],
    off: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        let end = self.off.checked_add(n).filter(|e| *e <= self.bytes.len());
        match end {
            Some(e) => {
                let s = &self.bytes[self.off..e];
                self.off = e;
                Ok(s)
            }
            None => Err(corrupt("column extends past the end of the tuple")),
        }
    }

    fn align(&mut self, a: usize) {
        self.off = align_up(self.off, a);
    }

    fn fixed<const N: usize>(&mut self, a: usize) -> Result<[u8; N]> {
        self.align(a);
        let s = self.take(N)?;
        let mut out = [0u8; N];
        out.copy_from_slice(s);
        Ok(out)
    }

    fn varlena(&mut self, a: usize) -> Result<std::borrow::Cow<'a, [u8]>> {
        if self.off >= self.bytes.len() {
            return Err(corrupt("varlena header past the end of the tuple"));
        }
        if self.bytes[self.off] == 0 {
            self.align(a);
        }
        if self.off >= self.bytes.len() {
            return Err(corrupt("varlena header past the end of the tuple"));
        }
        let b = self.bytes[self.off];
        if b == 0x01 {
            return Err(corrupt("external (TOAST) values are not supported"));
        }
        if b & 1 == 1 {
            let total = usize::from((b >> 1) & 0x7F);
            if total < 1 {
                return Err(corrupt("bad varlena length"));
            }
            self.off += 1;
            self.take(total - 1).map(std::borrow::Cow::Borrowed)
        } else if b & 3 == 2 {
            let h = self.take(4)?;
            let total = (u32::from_le_bytes([h[0], h[1], h[2], h[3]]) >> 2) as usize;
            if total < 8 {
                return Err(corrupt("bad varlena length"));
            }
            let body = self.take(total - 4)?;
            let raw = u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
            lz_decompress(&body[4..], raw).map(std::borrow::Cow::Owned)
        } else {
            let h = self.take(4)?;
            let v = u32::from_le_bytes([h[0], h[1], h[2], h[3]]);
            let total = (v >> 2) as usize;
            if total < 4 {
                return Err(corrupt("bad varlena length"));
            }
            self.take(total - 4).map(std::borrow::Cow::Borrowed)
        }
    }
}

fn utf8(b: &[u8]) -> Result<String> {
    String::from_utf8(b.to_vec()).map_err(|_| corrupt("text value is not valid UTF-8"))
}

/// Decodes a tuple. Damaged data gives `XX001`.
pub fn deform_tuple(desc: &TupleDesc, bytes: &[u8]) -> Result<Row> {
    let hdr = TupleHeader::read(bytes)?;
    let natts = usize::from(hdr.infomask2 & HEAP_NATTS_MASK);
    if natts > desc.attrs.len() {
        return Err(corrupt(format!(
            "tuple has {natts} columns but the descriptor has {}",
            desc.attrs.len()
        )));
    }
    let has_null = hdr.infomask & HEAP_HASNULL != 0;
    let mut r = Reader {
        bytes,
        off: usize::from(hdr.hoff),
    };
    let mut row: Row = Vec::with_capacity(desc.attrs.len());
    for (i, attr) in desc.attrs.iter().enumerate() {
        if i >= natts {
            row.push(Datum::Null);
            continue;
        }
        if has_null && bytes[SIZE_OF_HEAP_TUPLE_HEADER + i / 8] & (1 << (i % 8)) == 0 {
            row.push(Datum::Null);
            continue;
        }
        let a = attr.align as usize;
        let kind = kind_of(attr.type_oid).map_err(|e| corrupt(e.message.clone()))?;
        let d = match kind {
            Kind::NullOnly => return Err(corrupt("value stored in a NULL-only column")),
            Kind::Bool => Datum::Bool(r.take(1)?[0] != 0),
            Kind::Char => Datum::Char(r.take(1)?[0]),
            Kind::Int2 => Datum::Int2(i16::from_le_bytes(r.fixed::<2>(a)?)),
            Kind::Int4 => Datum::Int4(i32::from_le_bytes(r.fixed::<4>(a)?)),
            Kind::Int8 => Datum::Int8(i64::from_le_bytes(r.fixed::<8>(a)?)),
            Kind::Float4 => Datum::Float4(f32::from_bits(u32::from_le_bytes(r.fixed::<4>(a)?))),
            Kind::Float8 => Datum::Float8(f64::from_bits(u64::from_le_bytes(r.fixed::<8>(a)?))),
            Kind::Oid => Datum::Oid(u32::from_le_bytes(r.fixed::<4>(a)?)),
            Kind::Xid => Datum::Xid(u32::from_le_bytes(r.fixed::<4>(a)?)),
            Kind::Cid => Datum::Cid(u32::from_le_bytes(r.fixed::<4>(a)?)),
            Kind::Tid => {
                let b = r.fixed::<6>(a)?;
                Datum::Tid(Tid {
                    block: u32::from_le_bytes([b[0], b[1], b[2], b[3]]),
                    offset: u16::from_le_bytes([b[4], b[5]]),
                })
            }
            Kind::Name => {
                let b = r.take(64)?;
                let end = b.iter().position(|&c| c == 0).unwrap_or(64);
                Datum::Text(utf8(&b[..end])?)
            }
            Kind::Text => Datum::Text(utf8(&r.varlena(a)?)?),
            Kind::OidVector => {
                let b = r.varlena(a)?;
                if b.len() % 4 != 0 {
                    return Err(corrupt("oidvector length is not a multiple of 4"));
                }
                Datum::OidVector(
                    b.as_chunks::<4>()
                        .0
                        .iter()
                        .map(|c| u32::from_le_bytes(*c))
                        .collect(),
                )
            }
        };
        row.push(d);
    }
    Ok(row)
}

/// The fixed part of a tuple header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TupleHeader {
    pub xmin: Xid,
    pub xmax: Xid,
    pub cmin: CommandId,
    pub cmax: CommandId,
    pub ctid: Tid,
    pub infomask2: u16,
    pub infomask: u16,
    pub hoff: u8,
}

fn u16_at(b: &[u8], o: usize) -> u16 {
    u16::from_le_bytes([b[o], b[o + 1]])
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

fn u64_at(b: &[u8], o: usize) -> u64 {
    let mut a = [0u8; 8];
    a.copy_from_slice(&b[o..o + 8]);
    u64::from_le_bytes(a)
}

impl TupleHeader {
    /// Reads and validates the header (`XX001` when damaged).
    pub fn read(bytes: &[u8]) -> Result<TupleHeader> {
        if bytes.len() < SIZE_OF_HEAP_TUPLE_HEADER {
            return Err(corrupt(format!(
                "tuple of {} bytes is too short",
                bytes.len()
            )));
        }
        let h = TupleHeader {
            xmin: Xid(u64_at(bytes, 0)),
            xmax: Xid(u64_at(bytes, 8)),
            cmin: u32_at(bytes, 16),
            cmax: u32_at(bytes, 20),
            ctid: Tid {
                block: u32_at(bytes, 24),
                offset: u16_at(bytes, 28),
            },
            infomask2: u16_at(bytes, 30),
            infomask: u16_at(bytes, 32),
            hoff: bytes[34],
        };
        if h.xmin == Xid::INVALID {
            return Err(corrupt("xmin is invalid"));
        }
        if h.infomask & INFOMASK_FORBIDDEN != 0 || h.infomask2 & INFOMASK2_FORBIDDEN != 0 {
            return Err(corrupt("unused infomask bits are set"));
        }
        let natts = usize::from(h.infomask2 & HEAP_NATTS_MASK);
        let bm = if h.infomask & HEAP_HASNULL != 0 {
            bitmap_len(natts)
        } else {
            0
        };
        let hoff = usize::from(h.hoff);
        if hoff % 8 != 0 || hoff < SIZE_OF_HEAP_TUPLE_HEADER + bm || hoff > bytes.len() {
            return Err(corrupt(format!("bad header length {hoff}")));
        }
        Ok(h)
    }

    /// Writes the 35 fixed bytes (`bytes` must hold at least that many).
    pub fn write(&self, bytes: &mut [u8]) {
        bytes[0..8].copy_from_slice(&self.xmin.0.to_le_bytes());
        bytes[8..16].copy_from_slice(&self.xmax.0.to_le_bytes());
        bytes[16..20].copy_from_slice(&self.cmin.to_le_bytes());
        bytes[20..24].copy_from_slice(&self.cmax.to_le_bytes());
        bytes[24..28].copy_from_slice(&self.ctid.block.to_le_bytes());
        bytes[28..30].copy_from_slice(&self.ctid.offset.to_le_bytes());
        bytes[30..32].copy_from_slice(&self.infomask2.to_le_bytes());
        bytes[32..34].copy_from_slice(&self.infomask.to_le_bytes());
        bytes[34] = self.hoff;
    }

    pub fn xmax_invalid(&self) -> bool {
        self.xmax == Xid::INVALID || self.infomask & HEAP_XMAX_INVALID != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{Align, AttrDesc};

    fn desc(oids: &[u32]) -> TupleDesc {
        TupleDesc {
            attrs: oids.iter().map(|o| AttrDesc::from_type(*o)).collect(),
        }
    }

    fn w() -> WriteCtx {
        WriteCtx {
            xid: Xid(0x0102),
            cid: 7,
        }
    }

    fn example_desc() -> TupleDesc {
        desc(&[oid::INT4, oid::TEXT, oid::INT8, oid::BOOL])
    }

    fn example_row() -> Vec<Datum> {
        vec![
            Datum::Int4(1),
            Datum::Text("hello".into()),
            Datum::Null,
            Datum::Bool(true),
        ]
    }

    #[test]
    fn spec_example_bytes() {
        let t = form_tuple(&example_desc(), &example_row(), &w(), TupleFlags::default()).unwrap();
        assert_eq!(t.len(), 51);
        assert_eq!(&t[0..8], &0x0102u64.to_le_bytes());
        assert_eq!(&t[8..16], &[0; 8]);
        assert_eq!(&t[16..20], &7u32.to_le_bytes());
        assert_eq!(&t[20..24], &[0; 4]);
        assert_eq!(&t[30..32], &[0x04, 0x00]);
        assert_eq!(&t[32..34], &[0x03, 0x08]);
        assert_eq!(t[34], 40);
        assert_eq!(t[35], 0x0B);
        assert_eq!(&t[36..40], &[0; 4]);
        assert_eq!(&t[40..44], &[1, 0, 0, 0]);
        assert_eq!(t[44], 0x0D);
        assert_eq!(&t[45..50], b"hello");
        assert_eq!(t[50], 0x01);
    }

    #[test]
    fn roundtrip_example() {
        let t = form_tuple(&example_desc(), &example_row(), &w(), TupleFlags::default()).unwrap();
        assert_eq!(deform_tuple(&example_desc(), &t).unwrap(), example_row());
    }

    #[test]
    fn roundtrip_all_types() {
        let d = desc(&[
            oid::BOOL,
            oid::CHAR,
            oid::NAME,
            oid::INT2,
            oid::INT4,
            oid::INT8,
            oid::OID,
            oid::XID,
            oid::CID,
            oid::TID,
            oid::FLOAT4,
            oid::FLOAT8,
            oid::TEXT,
            oid::OIDVECTOR,
            oid::VARCHAR,
        ]);
        let long = "x".repeat(300);
        let row = vec![
            Datum::Bool(false),
            Datum::Char(b'r'),
            Datum::Text("pg_class".into()),
            Datum::Int2(-5),
            Datum::Int4(i32::MIN),
            Datum::Int8(i64::MAX),
            Datum::Oid(16384),
            Datum::Xid(9),
            Datum::Cid(3),
            Datum::Tid(Tid {
                block: 5,
                offset: 2,
            }),
            Datum::Float4(1.5),
            Datum::Float8(f64::NAN),
            Datum::Text(long.clone()),
            Datum::OidVector(vec![1, 2, 3]),
            Datum::Text("é".into()),
        ];
        let t = form_tuple(&d, &row, &w(), TupleFlags::default()).unwrap();
        let back = deform_tuple(&d, &t).unwrap();
        for (a, b) in row.iter().zip(&back) {
            match (a, b) {
                (Datum::Float8(x), Datum::Float8(y)) => assert_eq!(x.to_bits(), y.to_bits()),
                _ => assert_eq!(a, b),
            }
        }
    }

    #[test]
    fn four_byte_varlena_is_aligned() {
        let d = desc(&[oid::BOOL, oid::TEXT, oid::INT4]);
        let row = vec![
            Datum::Bool(true),
            Datum::Text("y".repeat(200)),
            Datum::Int4(77),
        ];
        let t = form_tuple(&d, &row, &w(), TupleFlags::default()).unwrap();
        assert_eq!(t[40], 1);
        assert_eq!(&t[41..44], &[0; 3]);
        assert_eq!(u32::from_le_bytes([t[44], t[45], t[46], t[47]]), 204 << 2);
        assert_eq!(deform_tuple(&d, &t).unwrap(), row);
    }

    #[test]
    fn many_columns_use_larger_hoff() {
        let d = desc(&[oid::BOOL; 50]);
        let mut row = vec![Datum::Bool(true); 50];
        row[3] = Datum::Null;
        let t = form_tuple(&d, &row, &w(), TupleFlags::default()).unwrap();
        assert_eq!(t[34], 48);
        assert_eq!(deform_tuple(&d, &t).unwrap(), row);
    }

    #[test]
    fn fewer_attrs_in_tuple_read_as_null() {
        let t = form_tuple(
            &desc(&[oid::INT4]),
            &[Datum::Int4(4)],
            &w(),
            TupleFlags::default(),
        )
        .unwrap();
        let got = deform_tuple(&desc(&[oid::INT4, oid::TEXT]), &t).unwrap();
        assert_eq!(got, vec![Datum::Int4(4), Datum::Null]);
        let err = deform_tuple(&desc(&[]), &t).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::DATA_CORRUPTED);
    }

    #[test]
    fn too_big_row_is_54000() {
        let d = desc(&[oid::TEXT]);
        let err =
            form_tuple(&d, &[Datum::Text(noisy(9000))], &w(), TupleFlags::default()).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert!(err.message.starts_with("row is too big: size "));
    }

    #[test]
    fn null_only_type_with_value_is_0a000() {
        let d = desc(&[oid::ACLITEM]);
        let err =
            form_tuple(&d, &[Datum::Text("x".into())], &w(), TupleFlags::default()).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        assert!(form_tuple(&d, &[Datum::Null], &w(), TupleFlags::default()).is_ok());
    }

    #[test]
    fn wrong_variant_is_internal_error() {
        let d = desc(&[oid::INT4]);
        let err = form_tuple(&d, &[Datum::Int8(1)], &w(), TupleFlags::default()).unwrap_err();
        assert_eq!(err.sqlstate, sqlstate::INTERNAL_ERROR);
    }

    #[test]
    fn updated_flag_and_header_roundtrip() {
        let d = desc(&[oid::INT4]);
        let t = form_tuple(&d, &[Datum::Int4(1)], &w(), TupleFlags { updated: true }).unwrap();
        let mut h = TupleHeader::read(&t).unwrap();
        assert_eq!(h.infomask, HEAP_XMAX_INVALID | HEAP_UPDATED);
        h.xmax = Xid(9);
        h.ctid = Tid {
            block: 3,
            offset: 4,
        };
        let mut t2 = t.clone();
        h.write(&mut t2);
        assert_eq!(TupleHeader::read(&t2).unwrap(), h);
    }

    #[test]
    fn damaged_headers_are_xx001() {
        let d = desc(&[oid::INT4]);
        let t = form_tuple(&d, &[Datum::Int4(1)], &w(), TupleFlags::default()).unwrap();
        let bad = |f: &dyn Fn(&mut Vec<u8>)| {
            let mut x = t.clone();
            f(&mut x);
            TupleHeader::read(&x).unwrap_err().sqlstate
        };
        assert_eq!(bad(&|x| x[32] |= 0x20), sqlstate::DATA_CORRUPTED);
        assert_eq!(bad(&|x| x[33] |= 0x80), sqlstate::DATA_CORRUPTED);
        assert_eq!(bad(&|x| x[31] |= 0x08), sqlstate::DATA_CORRUPTED);
        assert_eq!(bad(&|x| x[34] = 41), sqlstate::DATA_CORRUPTED);
        assert_eq!(bad(&|x| x[0..8].fill(0)), sqlstate::DATA_CORRUPTED);
        assert_eq!(bad(&|x| x.truncate(20)), sqlstate::DATA_CORRUPTED);
        // Column running past the end.
        let mut x = t.clone();
        x.truncate(42);
        assert_eq!(
            deform_tuple(&d, &x).unwrap_err().sqlstate,
            sqlstate::DATA_CORRUPTED
        );
    }

    fn noisy(n: usize) -> String {
        let mut x = 12345u32;
        (0..n)
            .map(|_| {
                x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                char::from(b'a' + ((x >> 24) % 26) as u8)
            })
            .collect()
    }

    #[test]
    fn large_compressible_value_round_trips() {
        let d = desc(&[oid::INT4, oid::TEXT]);
        for s in ["abc".repeat(10000), "xyz".repeat(20000)] {
            let row = vec![Datum::Int4(1), Datum::Text(s)];
            let t = form_tuple(&d, &row, &w(), TupleFlags::default()).unwrap();
            assert!(t.len() <= MAX_HEAP_TUPLE_SIZE);
            assert_eq!(deform_tuple(&d, &t).unwrap(), row);
        }
    }

    #[test]
    fn toast_pointer_is_xx001() {
        let d = desc(&[oid::TEXT]);
        let mut t =
            form_tuple(&d, &[Datum::Text("a".into())], &w(), TupleFlags::default()).unwrap();
        t[40] = 0x01;
        assert_eq!(
            deform_tuple(&d, &t).unwrap_err().sqlstate,
            sqlstate::DATA_CORRUPTED
        );
        let _ = Align::Int;
    }
}
