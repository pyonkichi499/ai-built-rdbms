//! 配列のテキスト入出力（`array_in` / `array_out`。`m5/05-types-core.md` §5.3、TY-5b）。
//!
//! PostgreSQL 17.11 の `array_in` / `ReadArrayDimensions` / `ReadArrayStr` / `ReadArrayToken` と
//! `array_out` の移植。要素の入力は読み進めながら行う（`{1,a,` は構文エラーより先に要素の
//! `22P02` になる）。M5 は 1 次元だけ作るので、構文の検査が済んだ後で 2 次元以上は `0A000`。

use super::array::{ArrayDim, ArrayValue, M5_MAX_NDIM, MAX_ARRAY_SIZE, MAXDIM};
use super::typeinfo::{array_kind, element_type};
use super::{Datum, Oid, SqlType, TypeEnv, io, typmod};
use crate::error::{Error, Result, sqlstate};

/// `scanner_isspace`（スペース、`\t`、`\n`、`\r`、`\v`、`\f`）。
fn is_space(b: u8) -> bool {
    matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c)
}

fn malformed(orig: &str, detail: &str) -> Error {
    Error::new(
        sqlstate::INVALID_TEXT_REPRESENTATION,
        format!("malformed array literal: \"{orig}\""),
    )
    .with_detail(detail)
}

fn too_many_dims() -> Error {
    Error::new(
        sqlstate::PROGRAM_LIMIT_EXCEEDED,
        format!("number of array dimensions exceeds the maximum allowed ({MAXDIM})"),
    )
}

fn multidim_unsupported() -> Error {
    Error::not_supported("multidimensional arrays are not supported yet")
}

/// 配列型 `array_ty` の入力関数。要素型の入力関数は `env` に従う。`array_ty.typmod` は要素に適用する。
pub fn array_in(s: &str, array_ty: SqlType, env: &TypeEnv<'_>) -> Result<Datum> {
    let elem = element_type(array_ty.oid).filter(|_| array_kind(array_ty.oid).is_some());
    let Some(elem) = elem else {
        return Err(Error::internal(format!(
            "array_in called for non-array type {}",
            array_ty.oid
        )));
    };
    Parser {
        s,
        b: s.as_bytes(),
        pos: 0,
        elem: SqlType::new(elem, array_ty.typmod),
        env,
    }
    .run()
}

struct Parser<'a, 'e> {
    s: &'a str,
    b: &'a [u8],
    pos: usize,
    elem: SqlType,
    env: &'e TypeEnv<'e>,
}

enum Token {
    LevelStart,
    LevelEnd,
    Delim,
    Elem(String),
    ElemNull,
}

impl Parser<'_, '_> {
    fn peek(&self) -> Option<u8> {
        self.b.get(self.pos).copied()
    }

    fn skip_space(&mut self) {
        while self.peek().is_some_and(is_space) {
            self.pos += 1;
        }
    }

    fn run(mut self) -> Result<Datum> {
        let spec = self.read_dimensions()?;
        if spec.is_empty() {
            self.skip_space();
            if self.peek() != Some(b'{') {
                return Err(malformed(
                    self.s,
                    "Array value must start with \"{\" or dimension information.",
                ));
            }
        } else {
            if self.peek() != Some(b'=') {
                return Err(malformed(self.s, "Missing \"=\" after array dimensions."));
            }
            self.pos += 1;
            self.skip_space();
            if self.peek() != Some(b'{') {
                return Err(malformed(self.s, "Array contents must start with \"{\"."));
            }
        }
        let (items, found) = self.read_items(&spec)?;
        // 閉じ括弧の後は空白だけ。
        self.skip_space();
        if self.pos < self.b.len() {
            return Err(malformed(self.s, "Junk after closing right brace."));
        }
        if items.is_empty() {
            return Ok(Datum::Array(Box::new(ArrayValue::empty(self.elem.oid))));
        }
        if found.len() > M5_MAX_NDIM {
            return Err(multidim_unsupported());
        }
        let lbound = spec.first().map_or(1, |d| d.lbound);
        let a = ArrayValue::from_vec(self.elem.oid, lbound, items)?;
        Ok(Datum::Array(Box::new(a)))
    }

    /// `[n]` / `[m:n]` の並び。無ければ空。
    fn read_dimensions(&mut self) -> Result<Vec<ArrayDim>> {
        let mut dims: Vec<ArrayDim> = Vec::new();
        loop {
            self.skip_space();
            if self.peek() != Some(b'[') {
                break;
            }
            if dims.len() >= MAXDIM {
                return Err(too_many_dims());
            }
            self.pos += 1;
            let Some(first) = self.read_dim_int()? else {
                return Err(malformed(
                    self.s,
                    "\"[\" must introduce explicitly-specified array dimensions.",
                ));
            };
            let (lb, ub) = if self.peek() == Some(b':') {
                self.pos += 1;
                let Some(ub) = self.read_dim_int()? else {
                    return Err(malformed(self.s, "Missing array dimension value."));
                };
                (first, ub)
            } else {
                (1, first)
            };
            if self.peek() != Some(b']') {
                return Err(malformed(self.s, "Missing \"]\" after array dimensions."));
            }
            self.pos += 1;
            if ub < lb {
                return Err(Error::new(
                    sqlstate::ARRAY_SUBSCRIPT_ERROR,
                    "upper bound cannot be less than lower bound",
                ));
            }
            if ub == i32::MAX {
                return Err(Error::new(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    format!("array upper bound is too large: {ub}"),
                ));
            }
            let len = i64::from(ub) - i64::from(lb) + 1;
            if len > MAX_ARRAY_SIZE as i64 {
                return Err(Error::new(
                    sqlstate::PROGRAM_LIMIT_EXCEEDED,
                    format!("array size exceeds the maximum allowed ({MAX_ARRAY_SIZE})"),
                ));
            }
            dims.push(ArrayDim {
                len: i32::try_from(len).unwrap_or(i32::MAX),
                lbound: lb,
            });
        }
        Ok(dims)
    }

    /// `ReadDimensionInt`: 符号と数字だけ。数字が無ければ `None`。
    fn read_dim_int(&mut self) -> Result<Option<i32>> {
        let start = self.pos;
        let mut p = self.pos;
        if matches!(self.b.get(p), Some(b'+' | b'-')) {
            p += 1;
        }
        let digits_start = p;
        while self.b.get(p).is_some_and(u8::is_ascii_digit) {
            p += 1;
        }
        if p == digits_start {
            return Ok(None);
        }
        self.pos = p;
        self.s[start..p].parse::<i32>().map(Some).map_err(|_| {
            Error::new(
                sqlstate::PROGRAM_LIMIT_EXCEEDED,
                "array bound is out of integer range",
            )
        })
    }

    /// `ReadArrayStr`。`spec` は明示された次元（空なら無し）。戻り値は要素の平坦な並びと、数えた各次元の長さ。
    fn read_items(&mut self, spec: &[ArrayDim]) -> Result<(Vec<Datum>, Vec<i32>)> {
        let mismatch = |p: &Self| {
            let detail = if spec.is_empty() {
                "Multidimensional arrays must have sub-arrays with matching dimensions."
            } else {
                "Specified array dimensions do not match array contents."
            };
            malformed(p.s, detail)
        };
        let frozen_at_start = !spec.is_empty();
        let mut ndim = spec.len();
        let mut ndim_frozen = frozen_at_start;
        let mut nest_level = 0usize;
        let mut nelems = [0i32; MAXDIM];
        let mut dim: [i32; MAXDIM] = [-1; MAXDIM];
        for (i, d) in spec.iter().enumerate() {
            dim[i] = d.len;
        }
        let mut expect_delim = false;
        let mut items: Vec<Datum> = Vec::new();
        loop {
            match self.read_token()? {
                Token::LevelStart => {
                    if expect_delim {
                        return Err(malformed(self.s, "Unexpected \"{\" character."));
                    }
                    if nest_level >= MAXDIM {
                        return Err(too_many_dims());
                    }
                    nest_level += 1;
                    if nest_level > ndim {
                        if ndim_frozen {
                            return Err(mismatch(self));
                        }
                        ndim = nest_level;
                    }
                }
                Token::LevelEnd => {
                    if nest_level == 0 {
                        return Err(malformed(self.s, "Unexpected \"}\" character."));
                    }
                    let level = nest_level - 1;
                    if nelems[level] > 0 && !expect_delim {
                        return Err(malformed(self.s, "Unexpected \"}\" character."));
                    }
                    if dim[level] < 0 {
                        dim[level] = nelems[level];
                    } else if dim[level] != nelems[level] {
                        return Err(mismatch(self));
                    }
                    nelems[level] = 0;
                    nest_level -= 1;
                    if nest_level > 0 {
                        nelems[nest_level - 1] += 1;
                    }
                    expect_delim = true;
                    if nest_level == 0 {
                        break;
                    }
                }
                Token::Delim => {
                    if !expect_delim {
                        return Err(malformed(self.s, "Unexpected \",\" character."));
                    }
                    expect_delim = false;
                }
                tok @ (Token::Elem(_) | Token::ElemNull) => {
                    if expect_delim {
                        return Err(malformed(self.s, "Unexpected array element."));
                    }
                    let d = match tok {
                        Token::Elem(text) => {
                            let d = io::input_text_env(&text, self.elem, self.env)?;
                            typmod::apply_typmod(d, self.elem, false)?
                        }
                        _ => Datum::Null,
                    };
                    ndim_frozen = true;
                    if nest_level == 0 || nest_level != ndim {
                        return Err(mismatch(self));
                    }
                    items.push(d);
                    nelems[nest_level - 1] += 1;
                    expect_delim = true;
                }
            }
        }
        // 数えた次元の長さ。空の配列（`{}`）は 1 次元分 0 として数えるが、呼び出し側は items で判定する。
        let found: Vec<i32> = dim[..ndim].to_vec();
        if !frozen_at_start || ndim == spec.len() {
            Ok((items, found))
        } else {
            Err(mismatch(self))
        }
    }

    /// `ReadArrayToken`。
    fn read_token(&mut self) -> Result<Token> {
        self.skip_space();
        let Some(c) = self.peek() else {
            return Err(malformed(self.s, "Unexpected end of input."));
        };
        match c {
            b'{' => {
                self.pos += 1;
                Ok(Token::LevelStart)
            }
            b'}' => {
                self.pos += 1;
                Ok(Token::LevelEnd)
            }
            b',' => {
                self.pos += 1;
                Ok(Token::Delim)
            }
            b'"' => {
                self.pos += 1;
                let mut out: Vec<u8> = Vec::new();
                loop {
                    match self.peek() {
                        None => return Err(malformed(self.s, "Unexpected end of input.")),
                        Some(b'\\') => {
                            self.pos += 1;
                            let Some(n) = self.peek() else {
                                return Err(malformed(self.s, "Unexpected end of input."));
                            };
                            out.push(n);
                            self.pos += 1;
                        }
                        Some(b'"') => {
                            self.pos += 1;
                            break;
                        }
                        Some(n) => {
                            out.push(n);
                            self.pos += 1;
                        }
                    }
                }
                self.skip_space();
                if !matches!(self.peek(), None | Some(b',' | b'}' | b'{')) {
                    return Err(malformed(self.s, "Incorrectly quoted array element."));
                }
                Ok(Token::Elem(into_string(out)))
            }
            _ => {
                let mut out: Vec<u8> = Vec::new();
                let mut end = 0usize;
                let mut has_escapes = false;
                loop {
                    match self.peek() {
                        None => return Err(malformed(self.s, "Unexpected end of input.")),
                        Some(b'\\') => {
                            self.pos += 1;
                            let Some(n) = self.peek() else {
                                return Err(malformed(self.s, "Unexpected end of input."));
                            };
                            out.push(n);
                            self.pos += 1;
                            end = out.len();
                            has_escapes = true;
                        }
                        Some(b'"') => {
                            return Err(malformed(self.s, "Incorrectly quoted array element."));
                        }
                        Some(b'{') => {
                            return Err(malformed(self.s, "Unexpected \"{\" character."));
                        }
                        Some(b',' | b'}') => break,
                        Some(n) => {
                            out.push(n);
                            self.pos += 1;
                            if !is_space(n) {
                                end = out.len();
                            }
                        }
                    }
                }
                out.truncate(end);
                if !has_escapes && out.eq_ignore_ascii_case(b"null") {
                    return Ok(Token::ElemNull);
                }
                Ok(Token::Elem(into_string(out)))
            }
        }
    }
}

/// 入力は有効な UTF-8 で、区切りは ASCII なので、取り出したバイト列も有効な UTF-8 になる。
fn into_string(bytes: Vec<u8>) -> String {
    String::from_utf8(bytes).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// `array_out`。要素は `output_text` で文字列にし、必要なら引用符をつける。
pub fn array_out(a: &ArrayValue, env: &TypeEnv<'_>) -> Result<String> {
    if a.items.is_empty() {
        return Ok("{}".to_owned());
    }
    let mut out = String::new();
    if a.dims.iter().any(|d| d.lbound != 1) {
        for d in &a.dims {
            let ub = i64::from(d.lbound) + i64::from(d.len) - 1;
            out.push_str(&format!("[{}:{ub}]", d.lbound));
        }
        out.push('=');
    }
    let ety = SqlType::of(a.elem_type);
    let mut idx = 0usize;
    write_level(a, 0, &mut idx, ety, env, &mut out)?;
    Ok(out)
}

fn write_level(
    a: &ArrayValue,
    level: usize,
    idx: &mut usize,
    ety: SqlType,
    env: &TypeEnv<'_>,
    out: &mut String,
) -> Result<()> {
    out.push('{');
    let n = a.dims[level].len;
    for i in 0..n {
        if i > 0 {
            out.push(',');
        }
        if level + 1 < a.dims.len() {
            write_level(a, level + 1, idx, ety, env, out)?;
        } else {
            let item = a
                .items
                .get(*idx)
                .ok_or_else(|| Error::internal("array has fewer items than its dimensions"))?;
            *idx += 1;
            match io::try_output_text_env(item, ety, env)? {
                None => out.push_str("NULL"),
                Some(text) => push_element(out, &text),
            }
        }
    }
    out.push('}');
    Ok(())
}

fn push_element(out: &mut String, text: &str) {
    let needs_quote = text.is_empty()
        || text.eq_ignore_ascii_case("null")
        || text
            .bytes()
            .any(|b| matches!(b, b'"' | b'\\' | b'{' | b'}' | b',') || is_space(b));
    if !needs_quote {
        out.push_str(text);
        return;
    }
    out.push('"');
    for ch in text.chars() {
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
}

/// 配列型 `ty` の入力が `DateStyle` などに依存しないか（`io::input_is_eager` の配列版）。
pub fn array_input_is_eager(array_oid: Oid) -> bool {
    element_type(array_oid).is_none_or(io::input_is_eager)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::oid;

    fn int4a() -> SqlType {
        SqlType::of(oid::INT4_ARRAY)
    }

    fn text_a() -> SqlType {
        SqlType::of(oid::TEXT_ARRAY)
    }

    fn parse(s: &str, ty: SqlType) -> Result<Datum> {
        array_in(s, ty, &TypeEnv::default())
    }

    fn roundtrip(s: &str, ty: SqlType) -> String {
        let d = parse(s, ty).unwrap();
        let Datum::Array(a) = d else { panic!("{d:?}") };
        array_out(&a, &TypeEnv::default()).unwrap()
    }

    fn err(s: &str, ty: SqlType) -> Error {
        parse(s, ty).unwrap_err()
    }

    #[test]
    fn basic_inputs_round_trip() {
        assert_eq!(roundtrip("{1,2,NULL}", int4a()), "{1,2,NULL}");
        assert_eq!(roundtrip("{ 1 , 2 }", int4a()), "{1,2}");
        assert_eq!(roundtrip("{\"1\",2}", int4a()), "{1,2}");
        assert_eq!(roundtrip("{a b}", text_a()), "{\"a b\"}");
        assert_eq!(roundtrip("{ a  b }", text_a()), "{\"a  b\"}");
        assert_eq!(roundtrip("{NULL}", text_a()), "{NULL}");
        assert_eq!(roundtrip("{null}", text_a()), "{NULL}");
        assert_eq!(roundtrip("{\"null\"}", text_a()), "{\"null\"}");
        assert_eq!(roundtrip("[0:2]={1,2,3}", int4a()), "[0:2]={1,2,3}");
        assert_eq!(roundtrip("{}", int4a()), "{}");
        assert_eq!(roundtrip("  {1}  ", int4a()), "{1}");
        assert_eq!(
            roundtrip(
                "{\"a b\",c,\"\",NULL,\"NULL\",\"a\\\"b\",\"a\\\\b\"}",
                text_a()
            ),
            "{\"a b\",c,\"\",NULL,\"NULL\",\"a\\\"b\",\"a\\\\b\"}"
        );
    }

    #[test]
    fn empty_forms_are_empty_arrays() {
        let Datum::Array(a) = parse("{{}}", int4a()).unwrap() else {
            panic!()
        };
        assert!(a.items.is_empty() && a.dims.is_empty());
    }

    #[test]
    fn errors_have_pg_messages() {
        let e = err("{a}", int4a());
        assert_eq!(e.sqlstate, sqlstate::INVALID_TEXT_REPRESENTATION);
        assert_eq!(e.message, "invalid input syntax for type integer: \"a\"");
        for (s, detail) in [
            ("{1,2", "Unexpected end of input."),
            (
                "1,2",
                "Array value must start with \"{\" or dimension information.",
            ),
            ("{1,,2}", "Unexpected \",\" character."),
            ("{1 2}", "Unexpected array element."),
            ("{1,}", "Unexpected \"}\" character."),
            (
                "{{1,2},{3}}",
                "Multidimensional arrays must have sub-arrays with matching dimensions.",
            ),
            ("{1}x", "Junk after closing right brace."),
            (
                "[1:2]={1}",
                "Specified array dimensions do not match array contents.",
            ),
            ("[1:2{1}", "Missing \"]\" after array dimensions."),
            ("[1:2]{1,2}", "Missing \"=\" after array dimensions."),
            (
                "[x]={1}",
                "\"[\" must introduce explicitly-specified array dimensions.",
            ),
            ("[1:]={1}", "Missing array dimension value."),
            ("{\"a\"b}", "Incorrectly quoted array element."),
        ] {
            let ty = if s.contains('"') { text_a() } else { int4a() };
            let e = err(s, ty);
            assert_eq!(e.sqlstate, sqlstate::INVALID_TEXT_REPRESENTATION, "{s}");
            assert_eq!(e.detail.as_deref(), Some(detail), "{s}");
            assert!(e.message.starts_with("malformed array literal"), "{s}");
        }
        assert_eq!(
            err("{1,2", int4a()).message,
            "malformed array literal: \"{1,2\""
        );
        let e = err("[2:1]={1}", int4a());
        assert_eq!(e.sqlstate, sqlstate::ARRAY_SUBSCRIPT_ERROR);
        let e = err("{{{{{{{1}}}}}}}", int4a());
        assert_eq!(e.sqlstate, sqlstate::PROGRAM_LIMIT_EXCEEDED);
        assert_eq!(
            e.message,
            "number of array dimensions exceeds the maximum allowed (6)"
        );
        let e = err("{{1,2},{3,4}}", int4a());
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
    }

    #[test]
    fn element_input_runs_while_reading() {
        // 要素の入力エラーが構文エラーより先に出る。
        let e = err("{1,a,", int4a());
        assert_eq!(e.message, "invalid input syntax for type integer: \"a\"");
    }

    #[test]
    fn escapes_and_quotes() {
        assert_eq!(roundtrip("{a\\ b}", text_a()), "{\"a b\"}");
        assert_eq!(roundtrip("{\\NULL}", text_a()), "{\"NULL\"}");
        assert_eq!(roundtrip("{\"x,y\"}", text_a()), "{\"x,y\"}");
        assert_eq!(roundtrip("{\"{\",\"}\"}", text_a()), "{\"{\",\"}\"}");
    }
}
