//! テキスト形式の行の読み取りとフィールドの分割・復元（`m4/10-explain-copy-compat.md` §5.4）。
//!
//! `LineReader` は `CopyData` のバイト列を行に組み立てる（行の境界とチャンクの境界は無関係）。
//! `split_fields` / `RawField::classify` / `unescape` はフィールドの分割と復元、`validate_utf8` と
//! `check_field_count` は復元後の検査（エラーの文言は PostgreSQL 17 に合わせる）。

use crate::error::{Error, Result, sqlstate};

/// 未完の 1 行の上限（10 §5.4 の 7）。
pub const COPY_MAX_LINE: usize = 64 << 20;

/// 行の終端の種類。最初の行の終端で決まり、以降の行は同じでなければならない。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Eol {
    Nl,
    CrNl,
    Cr,
}

fn bad_format(msg: &str) -> Error {
    Error::new(sqlstate::BAD_COPY_FILE_FORMAT, msg)
}

fn literal_nl() -> Error {
    bad_format("literal newline found in data").with_hint("Use \"\\n\" to represent newline.")
}

fn literal_cr() -> Error {
    bad_format("literal carriage return found in data")
        .with_hint("Use \"\\r\" to represent carriage return.")
}

fn corrupt_marker() -> Error {
    bad_format("end-of-copy marker corrupt")
}

#[derive(Debug, Default)]
pub struct LineReader {
    buf: Vec<u8>,
    /// 次に読む行の先頭（`buf` の中）。
    start: usize,
    /// `buf` の中で走査済みの位置（未完の行の再走査を避ける）。常に `start` 以上。
    scanned: usize,
    eol: Option<Eol>,
    /// `\.` を見た後。以降のバイトは捨てる。
    done: bool,
    /// 読み終えた行数。
    lines: u64,
}

impl LineReader {
    pub fn new() -> Self {
        LineReader::default()
    }

    pub fn push(&mut self, chunk: &[u8]) {
        if self.done {
            return;
        }
        if self.start == self.buf.len() {
            self.buf.clear();
            self.start = 0;
            self.scanned = 0;
        } else if self.start >= 64 * 1024 && self.start * 2 >= self.buf.len() {
            self.buf.drain(..self.start);
            self.scanned -= self.start;
            self.start = 0;
        }
        self.buf.extend_from_slice(chunk);
    }

    /// 完成した次の行（終端を除く）。`None` はデータ不足（`at_eof` でなければ次の `push` を待つ）か終了。
    pub fn next_line(&mut self, at_eof: bool) -> Result<Option<Vec<u8>>> {
        let mut out = Vec::new();
        Ok(self.next_line_into(at_eof, &mut out)?.then_some(out))
    }

    /// `next_line` の、バッファを再利用する版。行があれば `out` を置き換えて `true`。
    #[allow(clippy::too_many_lines)]
    pub fn next_line_into(&mut self, at_eof: bool, out: &mut Vec<u8>) -> Result<bool> {
        if self.done {
            return Ok(false);
        }
        let len = self.buf.len();
        let mut i = self.scanned.max(self.start);
        // (行の終わり, 次の行の先頭, `\.` による終了か)
        let (line_end, next_start, end_marker) = loop {
            if i >= len {
                self.scanned = len;
                if at_eof && self.start < len {
                    break (len, len, false);
                }
                self.check_len(len)?;
                return Ok(false);
            }
            match self.buf[i] {
                b'\\' => {
                    let Some(&next) = self.buf.get(i + 1) else {
                        if at_eof {
                            i += 1;
                            continue;
                        }
                        self.scanned = i;
                        self.check_len(i)?;
                        return Ok(false);
                    };
                    if next != b'.' {
                        i += 2;
                        continue;
                    }
                    let Some(&t) = self.buf.get(i + 2) else {
                        if at_eof {
                            return Err(corrupt_marker());
                        }
                        self.scanned = i;
                        return Ok(false);
                    };
                    let (kind, tlen) = match t {
                        b'\n' => (Eol::Nl, 1),
                        b'\r' => match self.buf.get(i + 3) {
                            Some(b'\n') => (Eol::CrNl, 2),
                            Some(_) => (Eol::Cr, 1),
                            None if at_eof => (Eol::Cr, 1),
                            None => {
                                self.scanned = i;
                                return Ok(false);
                            }
                        },
                        _ => return Err(corrupt_marker()),
                    };
                    match self.eol {
                        None => self.eol = Some(kind),
                        Some(e) if e != kind => {
                            return Err(bad_format(
                                "end-of-copy marker does not match previous newline style",
                            ));
                        }
                        _ => {}
                    }
                    break (i, i + 2 + tlen, true);
                }
                b'\n' => {
                    match self.eol {
                        None => self.eol = Some(Eol::Nl),
                        Some(Eol::Nl) => {}
                        Some(_) => return Err(literal_nl()),
                    }
                    break (i, i + 1, false);
                }
                b'\r' => {
                    let (kind, tlen) = match self.buf.get(i + 1) {
                        Some(b'\n') => (Eol::CrNl, 2),
                        Some(_) => (Eol::Cr, 1),
                        None if at_eof => (Eol::Cr, 1),
                        None => {
                            self.scanned = i;
                            return Ok(false);
                        }
                    };
                    match self.eol {
                        None => self.eol = Some(kind),
                        Some(e) if e != kind => return Err(literal_cr()),
                        _ => {}
                    }
                    break (i, i + tlen, false);
                }
                _ => i += 1,
            }
        };
        self.check_len(line_end)?;
        let line = &self.buf[self.start..line_end];
        if end_marker {
            self.done = true;
            let has_line = !line.is_empty();
            if has_line {
                out.clear();
                out.extend_from_slice(line);
                self.lines += 1;
            }
            self.buf = Vec::new();
            self.start = 0;
            self.scanned = 0;
            return Ok(has_line);
        }
        out.clear();
        out.extend_from_slice(line);
        self.lines += 1;
        self.start = next_start;
        self.scanned = next_start;
        Ok(true)
    }

    fn check_len(&self, end: usize) -> Result<()> {
        if end.saturating_sub(self.start) > COPY_MAX_LINE {
            return Err(Error::new(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "COPY line is too long",
            ));
        }
        Ok(())
    }

    /// `\.` を見て終了したか。
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// 次に読む行の番号（1 始まり）。エラーの CONTEXT に使う。
    pub fn line_no(&self) -> u64 {
        self.lines + 1
    }
}

/// 区切り文字で分けた 1 フィールドの生のバイト列（エスケープを解く前）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RawField<'a> {
    pub raw: &'a [u8],
}

/// 復元されたフィールド。
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum Field {
    Null,
    Default,
    Value(Vec<u8>),
}

impl RawField<'_> {
    /// 生のバイト列が `null` と等しければ `Null`、`default` と等しければ `Default`、それ以外は `unescape`。
    pub fn classify(&self, null: &[u8], default: Option<&[u8]>) -> Field {
        if self.raw == null {
            Field::Null
        } else if default.is_some_and(|d| self.raw == d) {
            Field::Default
        } else {
            Field::Value(unescape(self.raw))
        }
    }
}

/// 行を `delim` で分ける。`\` の次のバイトは区切りとして扱わない。空行は 1 フィールド（空）。
pub fn split_fields(line: &[u8], delim: u8) -> Vec<RawField<'_>> {
    let mut fields = Vec::new();
    let mut start = 0;
    let mut i = 0;
    while i < line.len() {
        match line[i] {
            b'\\' => i += 2,
            b if b == delim => {
                fields.push(RawField {
                    raw: &line[start..i],
                });
                i += 1;
                start = i;
            }
            _ => i += 1,
        }
    }
    fields.push(RawField {
        raw: &line[start.min(line.len())..],
    });
    fields
}

fn hex_val(b: u8) -> Option<u8> {
    char::from(b)
        .to_digit(16)
        .and_then(|d| u8::try_from(d).ok())
}

/// `\b \f \n \r \t \v`、`\` + 8 進 1〜3 桁、`\x` + 16 進 1〜2 桁、`\\`、その他の `\c` は `c`。行末の `\` はそのまま。
pub fn unescape(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut pos = 0;
    while pos < raw.len() {
        let byte = raw[pos];
        pos += 1;
        if byte != b'\\' {
            out.push(byte);
            continue;
        }
        let Some(&esc) = raw.get(pos) else {
            out.push(b'\\');
            break;
        };
        pos += 1;
        match esc {
            b'b' => out.push(0x08),
            b'f' => out.push(0x0C),
            b'n' => out.push(b'\n'),
            b'r' => out.push(b'\r'),
            b't' => out.push(b'\t'),
            b'v' => out.push(0x0B),
            b'0'..=b'7' => {
                let mut val = u32::from(esc - b'0');
                for _ in 0..2 {
                    match raw.get(pos) {
                        Some(digit @ b'0'..=b'7') => {
                            val = val * 8 + u32::from(digit - b'0');
                            pos += 1;
                        }
                        _ => break,
                    }
                }
                out.push((val & 0xFF) as u8);
            }
            b'x' => match raw.get(pos).copied().and_then(hex_val) {
                Some(h1) => {
                    pos += 1;
                    let mut val = h1;
                    if let Some(h2) = raw.get(pos).copied().and_then(hex_val) {
                        val = val * 16 + h2;
                        pos += 1;
                    }
                    out.push(val);
                }
                None => out.push(b'x'),
            },
            other => out.push(other),
        }
    }
    out
}

/// 復元後の値が UTF-8 で NUL を含まないことを検査する（`22021`。10 §5.4 の 4）。
pub fn validate_utf8(bytes: &[u8]) -> Result<()> {
    let nul = bytes.iter().position(|&b| b == 0);
    let head = &bytes[..nul.unwrap_or(bytes.len())];
    if let Err(e) = std::str::from_utf8(head) {
        let at = e.valid_up_to();
        let n = e.error_len().unwrap_or(head.len() - at).clamp(1, 4);
        let hex: Vec<String> = head[at..at + n]
            .iter()
            .map(|b| format!("0x{b:02x}"))
            .collect();
        return Err(Error::new(
            sqlstate::CHARACTER_NOT_IN_REPERTOIRE,
            format!(
                "invalid byte sequence for encoding \"UTF8\": {}",
                hex.join(" ")
            ),
        ));
    }
    if nul.is_some() {
        return Err(Error::new(
            sqlstate::CHARACTER_NOT_IN_REPERTOIRE,
            "invalid byte sequence for encoding \"UTF8\": 0x00",
        ));
    }
    Ok(())
}

/// フィールド数が列数と違えば `22P04`（足りない最初の列名、または余り）。
pub fn check_field_count(nfields: usize, columns: &[&str]) -> Result<()> {
    match nfields.cmp(&columns.len()) {
        std::cmp::Ordering::Less => Err(bad_format(&format!(
            "missing data for column \"{}\"",
            columns[nfields]
        ))),
        std::cmp::Ordering::Greater => Err(bad_format("extra data after last expected column")),
        std::cmp::Ordering::Equal => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    type Outcome = (Vec<Vec<u8>>, Option<(&'static str, String)>);

    fn raws<'a>(fs: &[RawField<'a>]) -> Vec<&'a [u8]> {
        fs.iter().map(|f| f.raw).collect()
    }

    #[test]
    fn splits_on_unescaped_delimiters() {
        assert_eq!(
            raws(&split_fields(b"a\tb\\\tc\td", b'\t')),
            vec![&b"a"[..], b"b\\\tc", b"d"]
        );
        assert_eq!(raws(&split_fields(b"", b'\t')), vec![&b""[..]]);
        assert_eq!(raws(&split_fields(b"a\t", b'\t')), vec![&b"a"[..], b""]);
        // 行末の `\` は 1 バイトだけ。
        assert_eq!(raws(&split_fields(b"a\\", b'\t')), vec![&b"a\\"[..]]);
        assert_eq!(
            raws(&split_fields(b"a,b\\,c", b',')),
            vec![&b"a"[..], b"b\\,c"]
        );
    }

    #[test]
    fn unescape_follows_postgresql() {
        assert_eq!(unescape(b"a\\tb\\x41\\101\\q"), b"a\tbAAq");
        assert_eq!(unescape(b"\\\\N"), b"\\N");
        assert_eq!(unescape(b"x\\"), b"x\\");
        assert_eq!(unescape(b"\\xZ"), b"xZ");
        assert_eq!(unescape(b"\\x4"), b"\x04");
        assert_eq!(unescape(b"\\1012"), b"A2");
        assert_eq!(unescape(b"\\b\\f\\n\\r\\v"), b"\x08\x0c\n\r\x0b");
        assert_eq!(unescape(b"\\400"), b"\0");
        assert_eq!(unescape(b"\\x414"), b"A4");
        assert_eq!(unescape(b"a\\\tb"), b"a\tb");
    }

    #[test]
    fn classify_null_and_default() {
        let f = |raw: &'static [u8]| RawField { raw }.classify(b"\\N", Some(b"DEF"));
        assert_eq!(f(b"\\N"), Field::Null);
        assert_eq!(f(b"DEF"), Field::Default);
        assert_eq!(f(b"\\\\N"), Field::Value(b"\\N".to_vec()));
        let g = RawField { raw: b"" }.classify(b"", None);
        assert_eq!(g, Field::Null);
    }

    fn drain(r: &mut LineReader, eof: bool, out: &mut Vec<Vec<u8>>) -> Result<()> {
        while let Some(l) = r.next_line(eof)? {
            out.push(l);
        }
        Ok(())
    }

    /// `chunks` ごとに push して、行の並びと最初のエラーを返す。
    fn run(chunks: &[&[u8]]) -> Outcome {
        let mut r = LineReader::new();
        let mut out = Vec::new();
        let fail = |e: Error| Some((e.sqlstate.code(), e.message));
        for c in chunks {
            r.push(c);
            if let Err(e) = drain(&mut r, false, &mut out) {
                return (out, fail(e));
            }
        }
        if let Err(e) = drain(&mut r, true, &mut out) {
            return (out, fail(e));
        }
        (out, None)
    }

    fn lines(input: &[u8]) -> Vec<Vec<u8>> {
        let (l, e) = run(&[input]);
        assert!(e.is_none(), "{e:?}");
        l
    }

    fn v(s: &[&str]) -> Vec<Vec<u8>> {
        s.iter().map(|x| x.as_bytes().to_vec()).collect()
    }

    #[test]
    fn line_numbers_and_basic_lines() {
        let mut r = LineReader::new();
        r.push(b"1\t2\n3");
        assert_eq!(r.line_no(), 1);
        assert_eq!(r.next_line(false).unwrap(), Some(b"1\t2".to_vec()));
        assert_eq!(r.line_no(), 2);
        assert_eq!(r.next_line(false).unwrap(), None);
        assert_eq!(r.next_line(true).unwrap(), Some(b"3".to_vec()));
        assert_eq!(r.line_no(), 3);
        assert_eq!(r.next_line(true).unwrap(), None);
    }

    #[test]
    fn newline_styles() {
        assert_eq!(lines(b"a\nb\n"), v(&["a", "b"]));
        assert_eq!(lines(b"a\r\nb\r\n"), v(&["a", "b"]));
        assert_eq!(lines(b"a\rb\r"), v(&["a", "b"]));
        assert_eq!(lines(b"a\n\nb"), v(&["a", "", "b"]));
        assert_eq!(lines(b""), v(&[]));
    }

    #[test]
    fn mixed_newlines_are_errors() {
        let e = run(&[b"a\nb\r\n"]).1.unwrap();
        assert_eq!(e, ("22P04", "literal carriage return found in data".into()));
        let e = run(&[b"a\r\nb\n"]).1.unwrap();
        assert_eq!(e, ("22P04", "literal newline found in data".into()));
        assert_eq!(
            run(&[b"a\rb\n"]).1.unwrap().1,
            "literal newline found in data"
        );
        assert_eq!(
            run(&[b"a\nb\rc"]).1.unwrap().1,
            "literal carriage return found in data"
        );
        let mut r = LineReader::new();
        r.push(b"a\nb\r");
        r.next_line(false).unwrap();
        let err = r.next_line(true).unwrap_err();
        assert_eq!(
            err.hint.as_deref(),
            Some("Use \"\\r\" to represent carriage return.")
        );
    }

    #[test]
    fn end_marker() {
        assert_eq!(lines(b"1\n\\.\n2\n"), v(&["1"]));
        assert_eq!(lines(b"\\.\n"), v(&[]));
        assert_eq!(lines(b"22\tx\t5\\.\n"), v(&["22\tx\t5"]));
        assert_eq!(lines(b"1\r\n\\.\r\n2"), v(&["1"]));
        assert_eq!(lines(b"1\r\\.\rzzz"), v(&["1"]));
        assert_eq!(lines(b"\\.\r\nx"), v(&[]));
        let e = run(&[b"1\n\\.\r\n"]).1.unwrap();
        assert_eq!(
            e.1,
            "end-of-copy marker does not match previous newline style"
        );
        let e = run(&[b"\\.x\n"]).1.unwrap();
        assert_eq!(e, ("22P04", "end-of-copy marker corrupt".into()));
        assert_eq!(run(&[b"1\n\\."]).1.unwrap().1, "end-of-copy marker corrupt");
        let mut r = LineReader::new();
        r.push(b"\\.\n");
        assert_eq!(r.next_line(false).unwrap(), None);
        assert!(r.is_done());
        r.push(b"x\n");
        assert_eq!(r.next_line(true).unwrap(), None);
    }

    #[test]
    fn backslash_protects_next_byte() {
        assert_eq!(lines(b"a\\\nb\nc\n"), v(&["a\\\nb", "c"]));
        assert_eq!(lines(b"a\\\\\n\\.\n"), v(&["a\\\\"]));
        assert!(run(&[b"a\\.b\n"]).1.is_some());
        assert_eq!(lines(b"a\\"), v(&["a\\"]));
    }

    #[test]
    fn eof_without_terminator() {
        assert_eq!(lines(b"2\tx\t5"), v(&["2\tx\t5"]));
        assert_eq!(lines(b"a\r"), v(&["a"]));
    }

    #[test]
    fn line_too_long() {
        let mut r = LineReader::new();
        r.push(&vec![b'x'; COPY_MAX_LINE + 1]);
        let e = r.next_line(false).unwrap_err();
        assert_eq!(e.sqlstate.code(), "54000");
        assert_eq!(e.message, "COPY line is too long");
    }

    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            self.0 >> 33
        }
    }

    #[test]
    fn chunking_is_equivalent() {
        let inputs: Vec<&[u8]> = vec![
            b"1\tx\n2\ty\n\\.\n3\tz\n",
            b"a\r\nb\r\n\\.\r\n",
            b"a\rb\rc",
            b"a\\\nb\\\\\nc\\",
            b"1\n2\r\n3\n",
            b"1\n\\.x\n",
            b"1\n\\.\r\n",
            b"22\tx\t5\\.\n",
            b"\\.",
            b"\n\n\r\n",
            b"x\r",
            b"",
        ];
        let mut rng = Lcg(42);
        for inp in inputs {
            let whole = run(&[inp]);
            let bytes: Vec<&[u8]> = inp.chunks(1).collect();
            assert_eq!(run(&bytes), whole, "bytewise {inp:?}");
            for _ in 0..200 {
                let mut chunks: Vec<&[u8]> = Vec::new();
                let mut p = 0;
                while p < inp.len() {
                    let n = 1 + usize::try_from(rng.next() % 4).unwrap();
                    let e = (p + n).min(inp.len());
                    chunks.push(&inp[p..e]);
                    p = e;
                    if rng.next().is_multiple_of(5) {
                        chunks.push(&[]);
                    }
                }
                assert_eq!(run(&chunks), whole, "random {inp:?} {chunks:?}");
            }
        }
    }

    #[test]
    fn crnl_split_across_chunks() {
        assert_eq!(run(&[b"a\r", b"\nb\r\n"]).0, v(&["a", "b"]));
        assert_eq!(run(&[b"a\n\\.", b"\n"]).0, v(&["a"]));
        assert_eq!(run(&[b"a\n\\", b".\r", b"\n"]).0, v(&["a"]));
        assert_eq!(run(&[b"a\\", b"\nb\n"]).0, v(&["a\\\nb"]));
    }

    #[test]
    fn next_line_into_reuses_buffer() {
        let mut r = LineReader::new();
        r.push(b"ab\nc\n");
        let mut buf = Vec::new();
        assert!(r.next_line_into(false, &mut buf).unwrap());
        assert_eq!(buf, b"ab");
        assert!(r.next_line_into(false, &mut buf).unwrap());
        assert_eq!(buf, b"c");
        assert!(!r.next_line_into(true, &mut buf).unwrap());
    }

    #[test]
    fn compaction_keeps_lines_intact() {
        let mut r = LineReader::new();
        let mut got = 0;
        for i in 0..20000 {
            r.push(format!("{i}\tabcdefghij\nx").as_bytes());
            while r.next_line(false).unwrap().is_some() {
                got += 1;
            }
            r.push(b"\n");
            while r.next_line(false).unwrap().is_some() {
                got += 1;
            }
        }
        assert_eq!(got, 40000);
    }

    #[test]
    fn utf8_validation() {
        assert!(validate_utf8("あ".as_bytes()).is_ok());
        let e = validate_utf8(b"a\xffb").unwrap_err();
        assert_eq!(e.sqlstate.code(), "22021");
        assert_eq!(
            e.message,
            "invalid byte sequence for encoding \"UTF8\": 0xff"
        );
        let e = validate_utf8(b"\xe3\x81").unwrap_err();
        assert_eq!(
            e.message,
            "invalid byte sequence for encoding \"UTF8\": 0xe3 0x81"
        );
        let e = validate_utf8(b"a\0").unwrap_err();
        assert_eq!(
            e.message,
            "invalid byte sequence for encoding \"UTF8\": 0x00"
        );
    }

    #[test]
    fn field_count() {
        assert!(check_field_count(2, &["a", "b"]).is_ok());
        let e = check_field_count(1, &["a", "b"]).unwrap_err();
        assert_eq!(e.sqlstate.code(), "22P04");
        assert_eq!(e.message, "missing data for column \"b\"");
        let e = check_field_count(3, &["a", "b"]).unwrap_err();
        assert_eq!(e.message, "extra data after last expected column");
    }
}
