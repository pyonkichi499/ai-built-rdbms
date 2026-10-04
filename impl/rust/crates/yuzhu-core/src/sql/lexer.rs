//! Hand-written lexer following PostgreSQL's `scan.l`.
//!
//! - Whitespace, `--` comments and nested `/* */` comments are skipped.
//! - Unquoted identifiers are folded to lower case (ASCII only) and, like
//!   quoted identifiers, truncated to 63 bytes (`NAMEDATALEN - 1`).
//! - String constants: `'...'` (`''` escapes a quote, backslash is literal as
//!   with `standard_conforming_strings = on`), `E'...'` (backslash escapes),
//!   `N'...'` (lexed as the keyword `nchar` followed by a string, as
//!   PostgreSQL does), and `$tag$...$tag$`. Adjacent string constants
//!   separated by whitespace containing a newline are concatenated.
//! - Numbers: `1`, `1_000`, `0x1F`, `0o17`, `0b101`, `1.5`, `.5`, `1e10`.
//! - Operators follow PostgreSQL's rules for multi-character operators
//!   (a trailing `+`/`-` is split off unless the operator contains one of
//!   ``~!@#^&|`?%``; `--` and `/*` inside an operator start a comment).
//!
//! Lexing is done eagerly, but a lexical error is not reported immediately:
//! it becomes a [`TokenKind::LexError`] token so that a syntax error found
//! earlier by the parser wins, as with PostgreSQL's on-demand scanner.

use super::token::{Token, TokenKind};
use crate::error::{Error, Span, sqlstate};

/// Maximum identifier length in bytes (`NAMEDATALEN - 1`).
pub const MAX_IDENTIFIER_LEN: usize = 63;

/// Tokenizes `sql`. The returned vector always ends with a [`TokenKind::Eof`]
/// token, or with a [`TokenKind::LexError`] token if lexing failed; in the
/// latter case the error is returned as well.
pub fn tokenize(sql: &str) -> (Vec<Token>, Option<Error>) {
    let mut lexer = Lexer {
        sql,
        b: sql.as_bytes(),
        pos: 0,
    };
    let mut tokens = Vec::new();
    loop {
        match lexer.next_token() {
            Ok(tok) => {
                let eof = tok.kind == TokenKind::Eof;
                tokens.push(tok);
                if eof {
                    return (tokens, None);
                }
            }
            Err((start, err)) => {
                tokens.push(Token {
                    kind: TokenKind::LexError,
                    span: Span::new(to_u32(start), to_u32(start)),
                });
                return (tokens, Some(err));
            }
        }
    }
}

fn to_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

fn is_space(c: u8) -> bool {
    matches!(c, b' ' | b'\t' | b'\n' | b'\r' | b'\x0c' | b'\x0b')
}

fn is_newline(c: u8) -> bool {
    c == b'\n' || c == b'\r'
}

fn is_ident_start(c: u8) -> bool {
    c.is_ascii_alphabetic() || c == b'_' || c >= 0x80
}

fn is_ident_cont(c: u8) -> bool {
    is_ident_start(c) || c.is_ascii_digit() || c == b'$'
}

fn is_op_char(c: u8) -> bool {
    matches!(
        c,
        b'~' | b'!'
            | b'@'
            | b'#'
            | b'^'
            | b'&'
            | b'|'
            | b'`'
            | b'?'
            | b'+'
            | b'-'
            | b'*'
            | b'/'
            | b'%'
            | b'<'
            | b'>'
            | b'='
    )
}

/// Truncates an identifier to `MAX_IDENTIFIER_LEN` bytes at a character
/// boundary (PostgreSQL also emits a NOTICE; we cannot from the parser).
pub fn truncate_identifier(s: &mut String) {
    if s.len() > MAX_IDENTIFIER_LEN {
        let mut end = MAX_IDENTIFIER_LEN;
        while !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
    }
}

/// Converts digits in `base` to a decimal digit string.
fn to_decimal(digits: &str, base: u32) -> String {
    // Little-endian decimal digits.
    let mut acc: Vec<u32> = vec![0];
    for ch in digits.chars() {
        let mut carry = ch.to_digit(base).unwrap_or(0);
        for d in &mut acc {
            let v = *d * base + carry;
            *d = v % 10;
            carry = v / 10;
        }
        while carry > 0 {
            acc.push(carry % 10);
            carry /= 10;
        }
    }
    while acc.len() > 1 && acc.last() == Some(&0) {
        acc.pop();
    }
    acc.iter()
        .rev()
        .map(|d| char::from_digit(*d, 10).unwrap_or('0'))
        .collect()
}

type LexResult<T> = std::result::Result<T, (usize, Error)>;

struct Lexer<'a> {
    sql: &'a str,
    b: &'a [u8],
    pos: usize,
}

impl Lexer<'_> {
    fn peek_at(&self, i: usize) -> Option<u8> {
        self.b.get(i).copied()
    }

    /// `"{msg} at or near \"{text}\""` as a syntax error at `start`.
    fn near(&self, start: usize, end: usize, msg: &str) -> (usize, Error) {
        let end = self.char_end(end);
        let text = &self.sql[start..end];
        (
            start,
            Error::syntax_at(
                Span::new(to_u32(start), to_u32(end)),
                format!("{msg} at or near \"{text}\""),
            ),
        )
    }

    /// Rounds a byte offset up to a character boundary.
    fn char_end(&self, mut end: usize) -> usize {
        end = end.min(self.b.len());
        while !self.sql.is_char_boundary(end) {
            end += 1;
        }
        end
    }

    /// Skips whitespace and comments.
    fn skip_whitespace(&mut self) -> LexResult<()> {
        loop {
            let Some(c) = self.peek_at(self.pos) else {
                return Ok(());
            };
            if is_space(c) {
                self.pos += 1;
            } else if c == b'-' && self.peek_at(self.pos + 1) == Some(b'-') {
                while let Some(c) = self.peek_at(self.pos) {
                    if is_newline(c) {
                        break;
                    }
                    self.pos += 1;
                }
            } else if c == b'/' && self.peek_at(self.pos + 1) == Some(b'*') {
                let start = self.pos;
                let mut depth = 0usize;
                loop {
                    match (self.peek_at(self.pos), self.peek_at(self.pos + 1)) {
                        (Some(b'/'), Some(b'*')) => {
                            depth += 1;
                            self.pos += 2;
                        }
                        (Some(b'*'), Some(b'/')) => {
                            depth -= 1;
                            self.pos += 2;
                            if depth == 0 {
                                break;
                            }
                        }
                        (Some(_), _) => self.pos += 1,
                        (None, _) => {
                            return Err(self.near(start, self.b.len(), "unterminated /* comment"));
                        }
                    }
                }
            } else {
                return Ok(());
            }
        }
    }

    fn next_token(&mut self) -> LexResult<Token> {
        self.skip_whitespace()?;
        let start = self.pos;
        let Some(c) = self.peek_at(start) else {
            return Ok(self.token(TokenKind::Eof, start));
        };
        let next = self.peek_at(start + 1);
        let kind = match c {
            b'\'' => {
                self.pos += 1;
                self.quoted_string(start, false)?
            }
            b'e' | b'E' if next == Some(b'\'') => {
                self.pos += 2;
                self.quoted_string(start, true)?
            }
            b'n' | b'N' if next == Some(b'\'') => {
                // National character literal: the keyword NCHAR, then the
                // string as a separate token.
                self.pos += 1;
                TokenKind::Word {
                    value: "nchar".to_string(),
                    quoted: false,
                }
            }
            b'b' | b'B' | b'x' | b'X' if next == Some(b'\'') => {
                return Err((
                    start,
                    Error::not_supported("bit-string constants are not supported yet")
                        .with_span(Span::new(to_u32(start), to_u32(start + 1))),
                ));
            }
            b'u' | b'U'
                if next == Some(b'&') && matches!(self.peek_at(start + 2), Some(b'\'' | b'"')) =>
            {
                return Err((
                    start,
                    Error::not_supported("Unicode escape strings (U&) are not supported yet")
                        .with_span(Span::new(to_u32(start), to_u32(start + 1))),
                ));
            }
            b'"' => self.quoted_identifier(start)?,
            b'$' => self.dollar(start)?,
            b'0'..=b'9' => self.number(start)?,
            b'.' if next.is_some_and(|n| n.is_ascii_digit()) => self.number(start)?,
            _ if is_ident_start(c) => self.identifier(start),
            b':' => {
                if next == Some(b':') {
                    self.pos += 2;
                    TokenKind::Typecast
                } else if next == Some(b'=') {
                    self.pos += 2;
                    TokenKind::ColonEquals
                } else {
                    self.pos += 1;
                    TokenKind::Colon
                }
            }
            b',' | b'(' | b')' | b'[' | b']' | b'.' | b';' => {
                self.pos += 1;
                match c {
                    b',' => TokenKind::Comma,
                    b'(' => TokenKind::LParen,
                    b')' => TokenKind::RParen,
                    b'[' => TokenKind::LBracket,
                    b']' => TokenKind::RBracket,
                    b'.' => TokenKind::Dot,
                    _ => TokenKind::Semicolon,
                }
            }
            _ if is_op_char(c) => self.operator(start)?,
            _ => {
                let ch = self.sql[start..].chars().next().unwrap_or('?');
                self.pos += ch.len_utf8();
                TokenKind::Other(ch)
            }
        };
        Ok(self.token(kind, start))
    }

    fn token(&self, kind: TokenKind, start: usize) -> Token {
        Token {
            kind,
            span: Span::new(to_u32(start), to_u32(self.pos)),
        }
    }

    fn identifier(&mut self, start: usize) -> TokenKind {
        while self.peek_at(self.pos).is_some_and(is_ident_cont) {
            self.pos += 1;
        }
        let mut value: String = self.sql[start..self.pos]
            .chars()
            .map(|c| c.to_ascii_lowercase())
            .collect();
        truncate_identifier(&mut value);
        TokenKind::Word {
            value,
            quoted: false,
        }
    }

    fn quoted_identifier(&mut self, start: usize) -> LexResult<TokenKind> {
        self.pos += 1;
        let mut value = String::new();
        loop {
            let rest = &self.sql[self.pos..];
            let Some(i) = rest.find('"') else {
                return Err(self.near(start, self.b.len(), "unterminated quoted identifier"));
            };
            value.push_str(&rest[..i]);
            self.pos += i + 1;
            if self.peek_at(self.pos) == Some(b'"') {
                value.push('"');
                self.pos += 1;
            } else {
                break;
            }
        }
        if value.is_empty() {
            return Err(self.near(start, self.pos, "zero-length delimited identifier"));
        }
        truncate_identifier(&mut value);
        Ok(TokenKind::Word {
            value,
            quoted: true,
        })
    }

    /// After a closing quote at `self.pos`, returns the position just past
    /// the opening quote of a continuation literal (whitespace containing at
    /// least one newline, then `'`), if any.
    fn quote_continuation(&self) -> Option<usize> {
        let mut p = self.pos;
        let mut saw_newline = false;
        loop {
            match self.peek_at(p) {
                Some(c) if is_newline(c) => {
                    saw_newline = true;
                    p += 1;
                }
                Some(c) if is_space(c) => p += 1,
                Some(b'-') if self.peek_at(p + 1) == Some(b'-') => {
                    while self.peek_at(p).is_some_and(|c| !is_newline(c)) {
                        p += 1;
                    }
                }
                Some(b'\'') if saw_newline => return Some(p + 1),
                _ => return None,
            }
        }
    }

    /// Scans the body of a quoted string; `self.pos` is just past the
    /// opening quote.
    fn quoted_string(&mut self, start: usize, extended: bool) -> LexResult<TokenKind> {
        let mut buf: Vec<u8> = Vec::new();
        let mut saw_escape = false;
        loop {
            let Some(c) = self.peek_at(self.pos) else {
                return Err(self.near(start, self.b.len(), "unterminated quoted string"));
            };
            match c {
                b'\'' => {
                    if self.peek_at(self.pos + 1) == Some(b'\'') {
                        buf.push(b'\'');
                        self.pos += 2;
                    } else {
                        self.pos += 1;
                        match self.quote_continuation() {
                            Some(p) => self.pos = p,
                            None => break,
                        }
                    }
                }
                b'\\' if extended => {
                    saw_escape = true;
                    self.escape(&mut buf)?;
                }
                _ => {
                    buf.push(c);
                    self.pos += 1;
                }
            }
        }
        if saw_escape {
            check_utf8(&buf).map_err(|e| (start, e))?;
        }
        // Without escapes the bytes are slices of valid UTF-8 input.
        match String::from_utf8(buf) {
            Ok(s) => Ok(TokenKind::String(s)),
            Err(e) => Err((start, invalid_utf8_error(e.as_bytes()))),
        }
    }

    /// Processes one backslash escape in an `E'...'` string.
    fn escape(&mut self, buf: &mut Vec<u8>) -> LexResult<()> {
        let esc_start = self.pos;
        self.pos += 1;
        let Some(c) = self.peek_at(self.pos) else {
            return Err(self.near(esc_start - 1, self.b.len(), "unterminated quoted string"));
        };
        match c {
            b'b' => buf.push(b'\x08'),
            b'f' => buf.push(b'\x0c'),
            b'n' => buf.push(b'\n'),
            b'r' => buf.push(b'\r'),
            b't' => buf.push(b'\t'),
            b'0'..=b'7' => {
                let mut v: u32 = 0;
                let mut n = 0;
                while n < 3 && matches!(self.peek_at(self.pos), Some(b'0'..=b'7')) {
                    v = v * 8 + u32::from(self.b[self.pos] - b'0');
                    self.pos += 1;
                    n += 1;
                }
                buf.push((v & 0xff) as u8);
                return Ok(());
            }
            b'x' if self
                .peek_at(self.pos + 1)
                .is_some_and(|h| h.is_ascii_hexdigit()) =>
            {
                self.pos += 1;
                let mut v: u32 = 0;
                let mut n = 0;
                while n < 2
                    && self
                        .peek_at(self.pos)
                        .is_some_and(|h| h.is_ascii_hexdigit())
                {
                    v = v * 16 + char::from(self.b[self.pos]).to_digit(16).unwrap_or(0);
                    self.pos += 1;
                    n += 1;
                }
                buf.push((v & 0xff) as u8);
                return Ok(());
            }
            b'u' | b'U' => {
                let cp = self.unicode_escape(esc_start)?;
                let ch = if (0xD800..0xDC00).contains(&cp) {
                    // High surrogate: must be followed by a low surrogate.
                    let second = self.pos;
                    if self.peek_at(self.pos) == Some(b'\\')
                        && matches!(self.peek_at(self.pos + 1), Some(b'u' | b'U'))
                    {
                        self.pos += 1;
                        let low = self.unicode_escape(second)?;
                        if (0xDC00..0xE000).contains(&low) {
                            char::from_u32(0x10000 + ((cp - 0xD800) << 10) + (low - 0xDC00))
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                    .ok_or_else(|| {
                        self.near(second, second + 1, "invalid Unicode surrogate pair")
                    })?
                } else if (0xDC00..0xE000).contains(&cp) {
                    return Err(self.near(esc_start, self.pos, "invalid Unicode surrogate pair"));
                } else {
                    match char::from_u32(cp) {
                        Some(ch) if cp != 0 => ch,
                        _ => {
                            return Err(self.near(
                                esc_start,
                                self.pos,
                                "invalid Unicode escape value",
                            ));
                        }
                    }
                };
                let mut tmp = [0u8; 4];
                buf.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                return Ok(());
            }
            _ => {
                // Any other character stands for itself (keep multi-byte
                // characters intact).
                let ch = self.sql[self.pos..].chars().next().unwrap_or('\\');
                let mut tmp = [0u8; 4];
                buf.extend_from_slice(ch.encode_utf8(&mut tmp).as_bytes());
                self.pos += ch.len_utf8();
                return Ok(());
            }
        }
        self.pos += 1;
        Ok(())
    }

    /// Parses `uXXXX` / `UXXXXXXXX` at `self.pos` (the `u`/`U`).
    fn unicode_escape(&mut self, esc_start: usize) -> LexResult<u32> {
        let n = if self.b[self.pos] == b'u' { 4 } else { 8 };
        let digits_start = self.pos + 1;
        let ok = (0..n).all(|i| {
            self.peek_at(digits_start + i)
                .is_some_and(|h| h.is_ascii_hexdigit())
        });
        if !ok {
            let span = Span::new(to_u32(esc_start), to_u32(esc_start + 1));
            return Err((
                esc_start,
                Error::new(sqlstate::INVALID_ESCAPE_SEQUENCE, "invalid Unicode escape")
                    .with_hint("Unicode escapes must be \\uXXXX or \\UXXXXXXXX.")
                    .with_span(span),
            ));
        }
        let text = &self.sql[digits_start..digits_start + n];
        self.pos = digits_start + n;
        Ok(u32::from_str_radix(text, 16).unwrap_or(u32::MAX))
    }

    /// `$n` parameters and `$tag$...$tag$` strings.
    fn dollar(&mut self, start: usize) -> LexResult<TokenKind> {
        let next = self.peek_at(start + 1);
        if next.is_some_and(|c| c.is_ascii_digit()) {
            let mut p = start + 1;
            while self.peek_at(p).is_some_and(|c| c.is_ascii_digit())
                || (self.peek_at(p) == Some(b'_')
                    && self.peek_at(p + 1).is_some_and(|c| c.is_ascii_digit()))
            {
                p += 1;
            }
            if self.peek_at(p).is_some_and(is_ident_start) {
                return Err(self.near(start, p + 1, "trailing junk after parameter"));
            }
            self.pos = p;
            let digits: String = self.sql[start + 1..p]
                .chars()
                .filter(|c| *c != '_')
                .collect();
            return match digits.parse::<u32>() {
                Ok(n) if i32::try_from(n).is_ok() => Ok(TokenKind::Param(n)),
                _ => Err(self.near(start, p, "parameter number too large")),
            };
        }
        // Dollar-quote delimiter: $ [tag] $
        let mut p = start + 1;
        if self
            .peek_at(p)
            .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_' || c >= 0x80)
        {
            while self
                .peek_at(p)
                .is_some_and(|c| c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80)
            {
                p += 1;
            }
        }
        if self.peek_at(p) != Some(b'$') {
            self.pos = start + 1;
            return Ok(TokenKind::Other('$'));
        }
        let delim = &self.sql[start..=p];
        let body_start = p + 1;
        match self.sql[body_start..].find(delim) {
            Some(i) => {
                let body = self.sql[body_start..body_start + i].to_string();
                self.pos = body_start + i + delim.len();
                Ok(TokenKind::String(body))
            }
            None => Err(self.near(start, self.b.len(), "unterminated dollar-quoted string")),
        }
    }

    /// Scans `digit (_? digit)*` in `base` from `p`; returns the end.
    fn digits(&self, mut p: usize, base: u32) -> usize {
        let is_digit = |c: Option<u8>| c.is_some_and(|c| char::from(c).is_digit(base));
        if !is_digit(self.peek_at(p)) {
            return p;
        }
        while is_digit(self.peek_at(p))
            || (self.peek_at(p) == Some(b'_') && is_digit(self.peek_at(p + 1)))
        {
            p += 1;
        }
        p
    }

    fn junk_check(&self, start: usize, end: usize) -> LexResult<()> {
        if self.peek_at(end).is_some_and(is_ident_start) {
            return Err(self.near(start, end + 1, "trailing junk after numeric literal"));
        }
        Ok(())
    }

    fn number(&mut self, start: usize) -> LexResult<TokenKind> {
        let b1 = self.peek_at(start + 1);
        if self.b[start] == b'0'
            && let Some((base, what)) = match b1 {
                Some(b'x' | b'X') => Some((16, "invalid hexadecimal integer")),
                Some(b'o' | b'O') => Some((8, "invalid octal integer")),
                Some(b'b' | b'B') => Some((2, "invalid binary integer")),
                _ => None,
            }
        {
            let mut p = start + 2;
            if self.peek_at(p) == Some(b'_') {
                p += 1;
            }
            let end = self.digits(p, base);
            if end == p {
                return Err(self.near(start, p, what));
            }
            self.junk_check(start, end)?;
            self.pos = end;
            let digits: String = self.sql[p..end].chars().filter(|c| *c != '_').collect();
            return Ok(TokenKind::Integer(to_decimal(&digits, base)));
        }

        let mut p = self.digits(start, 10);
        let mut is_decimal = false;
        if self.peek_at(p) == Some(b'.') && self.peek_at(p + 1) != Some(b'.') {
            is_decimal = true;
            p = self.digits(p + 1, 10);
        }
        if matches!(self.peek_at(p), Some(b'e' | b'E')) {
            let mut q = p + 1;
            if matches!(self.peek_at(q), Some(b'+' | b'-')) {
                q += 1;
            }
            let end = self.digits(q, 10);
            if end == q {
                return Err(self.near(start, q, "trailing junk after numeric literal"));
            }
            is_decimal = true;
            p = end;
        }
        self.junk_check(start, p)?;
        self.pos = p;
        let text: String = self.sql[start..p].chars().filter(|c| *c != '_').collect();
        Ok(if is_decimal {
            TokenKind::Decimal(text)
        } else {
            TokenKind::Integer(to_decimal(&text, 10))
        })
    }

    fn operator(&mut self, start: usize) -> LexResult<TokenKind> {
        let mut end = start;
        while self.peek_at(end).is_some_and(is_op_char) {
            end += 1;
        }
        let full = &self.sql[start..end];
        // Embedded comment starts end the operator.
        let mut n = full.len();
        for pat in ["/*", "--"] {
            if let Some(i) = full.find(pat) {
                n = n.min(i);
            }
        }
        // A trailing + or - is split off unless the operator contains a
        // character that cannot be part of an SQL operator sequence.
        let bytes = full.as_bytes();
        if n > 1 && matches!(bytes[n - 1], b'+' | b'-') {
            let special = bytes[..n - 1].iter().any(|c| b"~!@#^&|`?%".contains(c));
            if !special {
                n -= 1;
                while n > 1 && matches!(bytes[n - 1], b'+' | b'-') {
                    n -= 1;
                }
            }
        }
        let op = &full[..n];
        if n >= 64 {
            return Err(self.near(start, start + n, "operator too long"));
        }
        self.pos = start + n;
        Ok(TokenKind::Op(if op == "!=" {
            "<>".to_string()
        } else {
            op.to_string()
        }))
    }
}

fn check_utf8(buf: &[u8]) -> crate::error::Result<()> {
    if buf.contains(&0) || std::str::from_utf8(buf).is_err() {
        return Err(invalid_utf8_error(buf));
    }
    Ok(())
}

/// `invalid byte sequence for encoding "UTF8": 0x..` (22021), reporting the
/// first offending sequence like `report_invalid_encoding`.
fn invalid_utf8_error(buf: &[u8]) -> Error {
    let bad = match buf.iter().position(|b| *b == 0) {
        Some(nul) if std::str::from_utf8(&buf[..nul]).is_ok() => nul,
        _ => match std::str::from_utf8(buf) {
            Err(e) => e.valid_up_to(),
            Ok(_) => buf.iter().position(|b| *b == 0).unwrap_or(0),
        },
    };
    let first = buf.get(bad).copied().unwrap_or(0);
    let len = match first {
        0xC0..=0xDF => 2,
        0xE0..=0xEF => 3,
        0xF0..=0xF7 => 4,
        _ => 1,
    };
    let shown: Vec<String> = buf[bad..]
        .iter()
        .take(len)
        .map(|b| format!("0x{b:02x}"))
        .collect();
    Error::new(
        sqlstate::CHARACTER_NOT_IN_REPERTOIRE,
        format!(
            "invalid byte sequence for encoding \"UTF8\": {}",
            shown.join(" ")
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(sql: &str) -> Vec<TokenKind> {
        let (toks, err) = tokenize(sql);
        assert!(err.is_none(), "unexpected lex error: {err:?}");
        toks.into_iter().map(|t| t.kind).collect()
    }

    fn word(s: &str) -> TokenKind {
        TokenKind::Word {
            value: s.into(),
            quoted: false,
        }
    }

    fn op(s: &str) -> TokenKind {
        TokenKind::Op(s.into())
    }

    fn lex_err(sql: &str) -> Error {
        let (toks, err) = tokenize(sql);
        assert_eq!(toks.last().unwrap().kind, TokenKind::LexError);
        let mut e = err.expect("expected a lex error");
        e.resolve_position(sql);
        e
    }

    #[test]
    fn words_and_folding() {
        assert_eq!(
            kinds("SELECT Foo \"Bar\" \"a\"\"b\" x$1 _y"),
            vec![
                word("select"),
                word("foo"),
                TokenKind::Word {
                    value: "Bar".into(),
                    quoted: true
                },
                TokenKind::Word {
                    value: "a\"b".into(),
                    quoted: true
                },
                word("x$1"),
                word("_y"),
                TokenKind::Eof
            ]
        );
        // Non-ASCII letters are identifier characters and are not folded.
        assert_eq!(kinds("ÉtÉ"), vec![word("ÉtÉ"), TokenKind::Eof]);
        // Truncation to 63 bytes.
        let long = "a".repeat(70);
        assert_eq!(kinds(&long), vec![word(&"a".repeat(63)), TokenKind::Eof]);
    }

    #[test]
    fn spans() {
        let (toks, _) = tokenize("  ab  'c'");
        assert_eq!(toks[0].span, Span::new(2, 4));
        assert_eq!(toks[1].span, Span::new(6, 9));
        assert_eq!(toks[2].span, Span::new(9, 9));
    }

    #[test]
    fn numbers() {
        assert_eq!(
            kinds("1 1.5 .5 5. 1e10 1.5E-3 1_000 0x1F 0o17 0b101 0X_ff"),
            vec![
                TokenKind::Integer("1".into()),
                TokenKind::Decimal("1.5".into()),
                TokenKind::Decimal(".5".into()),
                TokenKind::Decimal("5.".into()),
                TokenKind::Decimal("1e10".into()),
                TokenKind::Decimal("1.5E-3".into()),
                TokenKind::Integer("1000".into()),
                TokenKind::Integer("31".into()),
                TokenKind::Integer("15".into()),
                TokenKind::Integer("5".into()),
                TokenKind::Integer("255".into()),
                TokenKind::Eof
            ]
        );
        assert_eq!(
            kinds("0xFFFFFFFFFFFFFFFFFFFF"),
            vec![
                TokenKind::Integer("1208925819614629174706175".into()),
                TokenKind::Eof
            ]
        );
        assert_eq!(
            kinds("1..2"),
            vec![
                TokenKind::Integer("1".into()),
                TokenKind::Dot,
                TokenKind::Decimal(".2".into()),
                TokenKind::Eof
            ]
        );
        assert_eq!(
            kinds("1+2"),
            vec![
                TokenKind::Integer("1".into()),
                op("+"),
                TokenKind::Integer("2".into()),
                TokenKind::Eof
            ]
        );
    }

    #[test]
    fn number_errors() {
        let e = lex_err("SELECT 123abc");
        assert_eq!(
            e.message,
            "trailing junk after numeric literal at or near \"123a\""
        );
        assert_eq!(e.position, Some(8));
        assert_eq!(
            lex_err("SELECT 1.5e").message,
            "trailing junk after numeric literal at or near \"1.5e\""
        );
        assert_eq!(
            lex_err("SELECT 1e+").message,
            "trailing junk after numeric literal at or near \"1e+\""
        );
        assert_eq!(
            lex_err("SELECT 0x").message,
            "invalid hexadecimal integer at or near \"0x\""
        );
        assert_eq!(
            lex_err("SELECT 1_").message,
            "trailing junk after numeric literal at or near \"1_\""
        );
        assert_eq!(
            lex_err("SELECT $1a").message,
            "trailing junk after parameter at or near \"$1a\""
        );
    }

    #[test]
    fn strings() {
        assert_eq!(
            kinds("'it''s' 'a\\b'"),
            vec![
                TokenKind::String("it's".into()),
                TokenKind::String("a\\b".into()),
                TokenKind::Eof
            ]
        );
        assert_eq!(
            kinds(r"E'a\nb\tc\\d\'e\x41\101é\U0001F600\q'"),
            vec![
                TokenKind::String("a\nb\tc\\d'eAAé\u{1F600}q".into()),
                TokenKind::Eof
            ]
        );
        assert_eq!(
            kinds(r"e'😀'"),
            vec![TokenKind::String("\u{1F600}".into()), TokenKind::Eof]
        );
        assert_eq!(
            kinds("$$a'b$$ $tag$x$$y$tag$ $1"),
            vec![
                TokenKind::String("a'b".into()),
                TokenKind::String("x$$y".into()),
                TokenKind::Param(1),
                TokenKind::Eof
            ]
        );
        assert_eq!(
            kinds("N'x'"),
            vec![word("nchar"), TokenKind::String("x".into()), TokenKind::Eof]
        );
    }

    #[test]
    fn string_continuation() {
        assert_eq!(
            kinds("'a'\n'b'  \n -- c\n  'c'"),
            vec![TokenKind::String("abc".into()), TokenKind::Eof]
        );
        assert_eq!(
            kinds("'a' -- x\n'b'"),
            vec![TokenKind::String("ab".into()), TokenKind::Eof]
        );
        // No newline: two separate tokens (a syntax error in the parser).
        assert_eq!(
            kinds("'a' 'b'"),
            vec![
                TokenKind::String("a".into()),
                TokenKind::String("b".into()),
                TokenKind::Eof
            ]
        );
        // A block comment breaks the continuation.
        assert_eq!(kinds("'a'\n/* x */'b'").len(), 3);
        // E-string continuation keeps escape processing.
        assert_eq!(
            kinds("E'a'\n'\\n'"),
            vec![TokenKind::String("a\n".into()), TokenKind::Eof]
        );
        let (toks, _) = tokenize("'a'\n'b' x");
        assert_eq!(toks[0].span, Span::new(0, 7));
    }

    #[test]
    fn string_errors() {
        let e = lex_err("SELECT 'unterminated");
        assert_eq!(
            e.message,
            "unterminated quoted string at or near \"'unterminated\""
        );
        assert_eq!(e.position, Some(8));
        assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR);
        assert_eq!(
            lex_err("SELECT \"abc").message,
            "unterminated quoted identifier at or near \"\"abc\""
        );
        let e = lex_err("SELECT 1 AS \"\"");
        assert_eq!(
            e.message,
            "zero-length delimited identifier at or near \"\"\"\""
        );
        assert_eq!(e.position, Some(13));
        assert_eq!(
            lex_err("SELECT $$abc").message,
            "unterminated dollar-quoted string at or near \"$$abc\""
        );
        assert_eq!(
            lex_err("SELECT /* a /* b */ c").message,
            "unterminated /* comment at or near \"/* a /* b */ c\""
        );
        let e = lex_err(r"SELECT E'\xff'");
        assert_eq!(e.sqlstate, sqlstate::CHARACTER_NOT_IN_REPERTOIRE);
        assert_eq!(
            e.message,
            "invalid byte sequence for encoding \"UTF8\": 0xff"
        );
        assert_eq!(
            lex_err(r"SELECT E'\0'").message,
            "invalid byte sequence for encoding \"UTF8\": 0x00"
        );
        assert_eq!(
            lex_err(r"SELECT E'\u12'").sqlstate,
            sqlstate::INVALID_ESCAPE_SEQUENCE
        );
        assert_eq!(
            lex_err(r"SELECT E'\uD83D'").message,
            "invalid Unicode surrogate pair at or near \"'\""
        );
        assert_eq!(
            lex_err("SELECT X'1F'").sqlstate,
            sqlstate::FEATURE_NOT_SUPPORTED
        );
    }

    #[test]
    fn comments() {
        assert_eq!(
            kinds("a -- c\n b /* x /* y */ z */ c"),
            vec![word("a"), word("b"), word("c"), TokenKind::Eof]
        );
        assert_eq!(kinds("-- only\n/* comments */"), vec![TokenKind::Eof]);
    }

    #[test]
    fn operators() {
        assert_eq!(
            kinds("a<=b>=c<>d!=e||f::g:=h=>i"),
            vec![
                word("a"),
                op("<="),
                word("b"),
                op(">="),
                word("c"),
                op("<>"),
                word("d"),
                op("<>"),
                word("e"),
                op("||"),
                word("f"),
                TokenKind::Typecast,
                word("g"),
                TokenKind::ColonEquals,
                word("h"),
                op("=>"),
                word("i"),
                TokenKind::Eof
            ]
        );
        // Trailing + / - split off.
        assert_eq!(
            kinds("1=-2"),
            vec![
                TokenKind::Integer("1".into()),
                op("="),
                op("-"),
                TokenKind::Integer("2".into()),
                TokenKind::Eof
            ]
        );
        assert_eq!(
            kinds("1*-+2"),
            vec![
                TokenKind::Integer("1".into()),
                op("*"),
                op("-"),
                op("+"),
                TokenKind::Integer("2".into()),
                TokenKind::Eof
            ]
        );
        // ...but not when the operator has a "special" character.
        assert_eq!(kinds("@-")[0], op("@-"));
        // Comment start inside an operator.
        assert_eq!(
            kinds("1+--x\n2"),
            vec![
                TokenKind::Integer("1".into()),
                op("+"),
                TokenKind::Integer("2".into()),
                TokenKind::Eof
            ]
        );
        assert_eq!(
            kinds("(a,b);[]."),
            vec![
                TokenKind::LParen,
                word("a"),
                TokenKind::Comma,
                word("b"),
                TokenKind::RParen,
                TokenKind::Semicolon,
                TokenKind::LBracket,
                TokenKind::RBracket,
                TokenKind::Dot,
                TokenKind::Eof
            ]
        );
        assert_eq!(kinds("$")[0], TokenKind::Other('$'));
        assert_eq!(kinds("\\")[0], TokenKind::Other('\\'));
    }
}
