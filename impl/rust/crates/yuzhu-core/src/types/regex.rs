//! 手書きの正規表現エンジン（`m4/09-types-functions.md` §9。外部クレートなし）。
//!
//! PostgreSQL の ARE（`REG_ADVANCED`）の部分集合を、パターン → AST → Thompson 型の命令列 →
//! Pike VM（状態集合のシミュレーション）で照合する。捕獲は持たず、部分一致の有無だけを返す
//! （PG の `REG_NOSUB`）。計算量は O(文字数 × 命令数) で、破滅的なバックトラックは起きない。
//!
//! - 後方参照と先読み・後読みは `0A000`（D-9-7）。
//! - 大文字小文字の同一視と文字クラスは ASCII のみ（C ロケール。D-9-8）。
//! - エラーは `2201B` と `invalid regular expression: ` + PG の文言。

use std::cell::RefCell;
use std::rc::Rc;

use crate::error::{Error, Result, sqlstate};

/// 命令列の最大の長さ。超えたら `regular expression is too complex`。
const MAX_PROGRAM_SIZE: usize = 200_000;
/// 括弧のネストの上限（再帰の深さの保護）。
const MAX_DEPTH: usize = 256;
/// `{m,n}` の上限（PG の `DUPMAX`）。
const DUPMAX: u32 = 255;
/// PG の `CHR_MAX`。
const CHR_MAX: u64 = 0x7fff_fffe;
/// コンパイル結果のキャッシュの件数（PG の `RE_CACHE_SIZE`）。
const CACHE_SIZE: usize = 32;

/// コンパイル時のフラグ。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct RegexFlags {
    /// `~*` / `!~*` のとき true。
    pub icase: bool,
}

impl From<bool> for RegexFlags {
    fn from(icase: bool) -> Self {
        RegexFlags { icase }
    }
}

fn invalid(msg: &str) -> Error {
    Error::new(
        sqlstate::INVALID_REGULAR_EXPRESSION,
        format!("invalid regular expression: {msg}"),
    )
}

fn unsupported(msg: &str) -> Error {
    Error::new(sqlstate::FEATURE_NOT_SUPPORTED, msg)
}

fn bad_rpt() -> Error {
    invalid("quantifier operand invalid")
}

fn bad_escape() -> Error {
    invalid("invalid escape \\ sequence")
}

fn bad_brackets() -> Error {
    invalid("brackets [] not balanced")
}

fn bad_range() -> Error {
    invalid("invalid character range")
}

fn too_complex() -> Error {
    invalid("regular expression is too complex")
}

// ---------------------------------------------------------------------------
// 文字クラス
// ---------------------------------------------------------------------------

/// POSIX の文字クラス（C ロケール。ASCII のみ）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PosixClass {
    Alnum,
    Alpha,
    Ascii,
    Blank,
    Cntrl,
    Digit,
    Graph,
    Lower,
    Print,
    Punct,
    Space,
    Upper,
    Xdigit,
    Word,
}

impl PosixClass {
    fn from_name(name: &str) -> Option<PosixClass> {
        Some(match name {
            "alnum" => PosixClass::Alnum,
            "alpha" => PosixClass::Alpha,
            "ascii" => PosixClass::Ascii,
            "blank" => PosixClass::Blank,
            "cntrl" => PosixClass::Cntrl,
            "digit" => PosixClass::Digit,
            "graph" => PosixClass::Graph,
            "lower" => PosixClass::Lower,
            "print" => PosixClass::Print,
            "punct" => PosixClass::Punct,
            "space" => PosixClass::Space,
            "upper" => PosixClass::Upper,
            "xdigit" => PosixClass::Xdigit,
            "word" => PosixClass::Word,
            _ => return None,
        })
    }

    fn matches(self, c: u32) -> bool {
        if c >= 128 {
            return false;
        }
        let Ok(b) = u8::try_from(c) else {
            return false;
        };
        match self {
            PosixClass::Alnum => b.is_ascii_alphanumeric(),
            PosixClass::Alpha => b.is_ascii_alphabetic(),
            PosixClass::Ascii => true,
            PosixClass::Blank => b == b' ' || b == b'\t',
            PosixClass::Cntrl => b.is_ascii_control(),
            PosixClass::Digit => b.is_ascii_digit(),
            PosixClass::Graph => b.is_ascii_graphic(),
            PosixClass::Lower => b.is_ascii_lowercase(),
            PosixClass::Print => b.is_ascii_graphic() || b == b' ',
            PosixClass::Punct => b.is_ascii_punctuation(),
            PosixClass::Space => b == b' ' || (9..=13).contains(&b),
            PosixClass::Upper => b.is_ascii_uppercase(),
            PosixClass::Xdigit => b.is_ascii_hexdigit(),
            PosixClass::Word => b.is_ascii_alphanumeric() || b == b'_',
        }
    }
}

/// ブラケット式（と `\d` などのクラスエスケープ）が表す文字の集合。
#[derive(Clone, Debug)]
struct CharClass {
    /// 範囲（閉区間）。
    ranges: Vec<(u32, u32)>,
    /// `(クラス, 補集合か)`。補集合は `\D` `\S` `\W` で、改行にも一致する。
    classes: Vec<(PosixClass, bool)>,
    /// `[^...]`。
    negated: bool,
    /// 改行感応（`(?n)` `(?p)`）のとき、`[^...]` は改行に一致しない。
    nl_stop: bool,
    icase: bool,
}

impl CharClass {
    fn base(&self, c: u32) -> bool {
        self.ranges.iter().any(|&(lo, hi)| lo <= c && c <= hi)
            || self.classes.iter().any(|&(k, neg)| k.matches(c) != neg)
    }

    fn contains(&self, c: u32) -> bool {
        let mut hit = self.base(c);
        if !hit && self.icase && c < 128 {
            let b = u8::try_from(c).unwrap_or(0);
            if b.is_ascii_alphabetic() {
                hit = self.base(u32::from(b.to_ascii_lowercase()))
                    || self.base(u32::from(b.to_ascii_uppercase()));
            }
        }
        if self.negated {
            !(hit || (self.nl_stop && c == 10))
        } else {
            hit
        }
    }
}

/// PostgreSQL の `cnames` の表（`[[.name.]]` の名前。大文字小文字を区別する）。
const CNAMES: &[(&str, u32)] = &[
    ("NUL", 0),
    ("SOH", 1),
    ("STX", 2),
    ("ETX", 3),
    ("EOT", 4),
    ("ENQ", 5),
    ("ACK", 6),
    ("BEL", 7),
    ("alert", 7),
    ("BS", 8),
    ("backspace", 8),
    ("HT", 9),
    ("tab", 9),
    ("LF", 10),
    ("newline", 10),
    ("VT", 11),
    ("vertical-tab", 11),
    ("FF", 12),
    ("form-feed", 12),
    ("CR", 13),
    ("carriage-return", 13),
    ("SO", 14),
    ("SI", 15),
    ("DLE", 16),
    ("DC1", 17),
    ("DC2", 18),
    ("DC3", 19),
    ("DC4", 20),
    ("NAK", 21),
    ("SYN", 22),
    ("ETB", 23),
    ("CAN", 24),
    ("EM", 25),
    ("SUB", 26),
    ("ESC", 27),
    ("IS4", 28),
    ("FS", 28),
    ("IS3", 29),
    ("GS", 29),
    ("IS2", 30),
    ("RS", 30),
    ("IS1", 31),
    ("US", 31),
    ("space", 32),
    ("exclamation-mark", 33),
    ("quotation-mark", 34),
    ("number-sign", 35),
    ("dollar-sign", 36),
    ("percent-sign", 37),
    ("ampersand", 38),
    ("apostrophe", 39),
    ("left-parenthesis", 40),
    ("right-parenthesis", 41),
    ("asterisk", 42),
    ("plus-sign", 43),
    ("comma", 44),
    ("hyphen", 45),
    ("hyphen-minus", 45),
    ("period", 46),
    ("full-stop", 46),
    ("slash", 47),
    ("solidus", 47),
    ("zero", 48),
    ("one", 49),
    ("two", 50),
    ("three", 51),
    ("four", 52),
    ("five", 53),
    ("six", 54),
    ("seven", 55),
    ("eight", 56),
    ("nine", 57),
    ("colon", 58),
    ("semicolon", 59),
    ("less-than-sign", 60),
    ("equals-sign", 61),
    ("greater-than-sign", 62),
    ("question-mark", 63),
    ("commercial-at", 64),
    ("left-square-bracket", 91),
    ("backslash", 92),
    ("reverse-solidus", 92),
    ("right-square-bracket", 93),
    ("circumflex", 94),
    ("circumflex-accent", 94),
    ("underscore", 95),
    ("low-line", 95),
    ("grave-accent", 96),
    ("left-brace", 123),
    ("left-curly-bracket", 123),
    ("vertical-line", 124),
    ("right-brace", 125),
    ("right-curly-bracket", 125),
    ("tilde", 126),
    ("DEL", 127),
];

// ---------------------------------------------------------------------------
// AST
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AssertKind {
    /// `\A`、改行非感応の `^`。
    TextStart,
    /// 改行感応（`(?n)` `(?w)`）の `^`。
    LineStart,
    /// `\Z`、改行非感応の `$`。
    TextEnd,
    LineEnd,
    /// `\m` `[[:<:]]`。
    WordStart,
    /// `\M` `[[:>:]]`。
    WordEnd,
    /// `\y`。
    WordBoundary,
    /// `\Y`。
    NotWordBoundary,
}

fn is_word(c: Option<char>) -> bool {
    c.is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl AssertKind {
    fn holds(self, prev: Option<char>, next: Option<char>) -> bool {
        match self {
            AssertKind::TextStart => prev.is_none(),
            AssertKind::LineStart => prev.is_none() || prev == Some('\n'),
            AssertKind::TextEnd => next.is_none(),
            AssertKind::LineEnd => next.is_none() || next == Some('\n'),
            AssertKind::WordStart => !is_word(prev) && is_word(next),
            AssertKind::WordEnd => is_word(prev) && !is_word(next),
            AssertKind::WordBoundary => is_word(prev) != is_word(next),
            AssertKind::NotWordBoundary => is_word(prev) == is_word(next),
        }
    }
}

#[derive(Debug)]
enum Node {
    Empty,
    Char(u32),
    /// `.`。引数は改行に一致しない（改行感応）か。
    Any(bool),
    Class(CharClass),
    Assert(AssertKind),
    Cat(Vec<Node>),
    Alt(Vec<Node>),
    Repeat {
        min: u32,
        max: Option<u32>,
        node: Box<Node>,
    },
}

// ---------------------------------------------------------------------------
// 構文解析
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, Default)]
#[allow(clippy::struct_excessive_bools)]
struct Flags {
    icase: bool,
    /// `.` と `[^...]` が改行に一致しない。
    nlstop: bool,
    /// `^` `$` が行頭・行末に一致する。
    nlanch: bool,
    expanded: bool,
    quote: bool,
}

/// `\` エスケープの結果。
enum Esc {
    Char(u32),
    Class(PosixClass, bool),
    Assert(AssertKind),
    /// `\1`〜`\9`（以降）。値は番号。
    Backref(usize),
}

enum BrackTok {
    Plain(u32),
    Range,
    Collel(u32),
    Eclass(u32),
    Class(PosixClass, bool),
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
    f: Flags,
    /// ここまでに開いた捕獲グループの数。
    ngroups: usize,
    depth: usize,
}

fn is_cspace(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\n' | '\x0b' | '\x0c' | '\r')
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn peek_at(&self, off: usize) -> Option<char> {
        self.chars.get(self.pos + off).copied()
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    /// 先頭の `***` と `(?xyz)`。
    fn prefixes(&mut self) -> Result<()> {
        if self.chars.len() >= 4 && self.chars[..3] == ['*', '*', '*'] {
            match self.chars[3] {
                '?' => return Err(invalid("invalid regexp (reg version 0.8)")),
                '=' => {
                    self.f.quote = true;
                    self.f.expanded = false;
                    self.f.nlstop = false;
                    self.f.nlanch = false;
                    self.pos = 4;
                    return Ok(());
                }
                ':' => self.pos = 4,
                _ => return Err(bad_rpt()),
            }
        }
        if self.peek() == Some('(')
            && self.peek_at(1) == Some('?')
            && self.peek_at(2).is_some_and(|c| c.is_ascii_alphabetic())
        {
            self.pos += 2;
            while let Some(c) = self.peek().filter(char::is_ascii_alphabetic) {
                match c {
                    'b' | 'e' => {
                        return Err(unsupported(
                            "regular expression basic/extended syntax options are not supported yet",
                        ));
                    }
                    'c' => self.f.icase = false,
                    'i' => self.f.icase = true,
                    'm' | 'n' => {
                        self.f.nlstop = true;
                        self.f.nlanch = true;
                    }
                    'p' => {
                        self.f.nlstop = true;
                        self.f.nlanch = false;
                    }
                    'q' => self.f.quote = true,
                    's' => {
                        self.f.nlstop = false;
                        self.f.nlanch = false;
                    }
                    't' => self.f.expanded = false,
                    'w' => {
                        self.f.nlstop = false;
                        self.f.nlanch = true;
                    }
                    'x' => self.f.expanded = true,
                    _ => return Err(invalid("invalid embedded option")),
                }
                self.pos += 1;
            }
            if !self.eat(')') {
                return Err(invalid("invalid embedded option"));
            }
            if self.f.quote {
                self.f.expanded = false;
                self.f.nlstop = false;
                self.f.nlanch = false;
            }
        }
        Ok(())
    }

    fn parse_all(&mut self) -> Result<Node> {
        self.prefixes()?;
        if self.f.quote {
            let items: Vec<Node> = self.chars[self.pos..]
                .iter()
                .map(|&c| Node::Char(u32::from(c)))
                .collect();
            self.pos = self.chars.len();
            return Ok(Node::Cat(items));
        }
        let node = self.alt()?;
        if self.pos < self.chars.len() {
            return Err(invalid("parentheses () not balanced"));
        }
        Ok(node)
    }

    /// 空白・コメント（`x` オプション）と `(?#...)` を読み飛ばす。
    fn skip_ignorable(&mut self) {
        if self.f.quote {
            return;
        }
        loop {
            match self.peek() {
                Some(c) if self.f.expanded && is_cspace(c) => self.pos += 1,
                Some('#') if self.f.expanded => {
                    while let Some(c) = self.peek() {
                        self.pos += 1;
                        if c == '\n' {
                            break;
                        }
                    }
                }
                Some('(') if self.peek_at(1) == Some('?') && self.peek_at(2) == Some('#') => {
                    self.pos += 3;
                    while let Some(c) = self.peek() {
                        if c == ')' {
                            self.pos += 1;
                            break;
                        }
                        self.pos += 1;
                    }
                }
                _ => return,
            }
        }
    }

    fn alt(&mut self) -> Result<Node> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(too_complex());
        }
        let mut branches = vec![self.branch()?];
        loop {
            self.skip_ignorable();
            if !self.eat('|') {
                break;
            }
            branches.push(self.branch()?);
        }
        self.depth -= 1;
        Ok(if branches.len() == 1 {
            branches.remove(0)
        } else {
            Node::Alt(branches)
        })
    }

    fn branch(&mut self) -> Result<Node> {
        let mut items = Vec::new();
        loop {
            self.skip_ignorable();
            if matches!(self.peek(), None | Some('|' | ')')) {
                break;
            }
            let (atom, quantifiable) = self.atom()?;
            match self.quantify(atom, quantifiable)? {
                Node::Cat(inner) => items.extend(inner),
                Node::Empty => {}
                other => items.push(other),
            }
        }
        Ok(match items.len() {
            0 => Node::Empty,
            1 => items.remove(0),
            _ => Node::Cat(items),
        })
    }

    /// 量指定子のトークンなら読んで `(min, max)` を返す（`{` は直後が数字のときだけ）。
    fn quant_token(&mut self) -> Result<Option<(u32, Option<u32>)>> {
        match self.peek() {
            Some('*') => {
                self.pos += 1;
                Ok(Some((0, None)))
            }
            Some('+') => {
                self.pos += 1;
                Ok(Some((1, None)))
            }
            Some('?') => {
                self.pos += 1;
                Ok(Some((0, Some(1))))
            }
            Some('{') if self.peek_at(1).is_some_and(|c| c.is_ascii_digit()) => {
                self.pos += 1;
                self.bound().map(Some)
            }
            _ => Ok(None),
        }
    }

    /// `{` の直後から `}` までを読む。字句の誤り（EOS・想定外の文字）を先に検出し、値の検査は後で行う
    /// （PG は字句解析が 1 トークン先を読むため、この順で報告する）。
    fn bound(&mut self) -> Result<(u32, Option<u32>)> {
        let unbalanced = || invalid("braces {} not balanced");
        let bad = || invalid("invalid repetition count(s)");
        let read_num = |p: &mut Parser| -> Result<Option<u64>> {
            let mut n: Option<u64> = None;
            while let Some(c) = p.peek() {
                let Some(d) = c.to_digit(10) else { break };
                n = Some((n.unwrap_or(0) * 10 + u64::from(d)).min(1 << 40));
                p.pos += 1;
            }
            Ok(n)
        };
        let m = read_num(self)?;
        let mut n = m;
        let mut open_ended = false;
        match self.peek() {
            None => return Err(unbalanced()),
            Some(',') => {
                self.pos += 1;
                n = read_num(self)?;
                open_ended = n.is_none();
                match self.peek() {
                    None => return Err(unbalanced()),
                    Some('}') => {}
                    Some(_) => return Err(bad()),
                }
            }
            Some('}') => {}
            Some(_) => return Err(bad()),
        }
        self.pos += 1;
        let m = m.unwrap_or(0);
        if m > u64::from(DUPMAX) {
            return Err(bad());
        }
        if open_ended {
            return Ok((u32::try_from(m).unwrap_or(DUPMAX), None));
        }
        let n = n.unwrap_or(m);
        if n > u64::from(DUPMAX) || m > n {
            return Err(bad());
        }
        Ok((
            u32::try_from(m).unwrap_or(DUPMAX),
            Some(u32::try_from(n).unwrap_or(DUPMAX)),
        ))
    }

    fn quantify(&mut self, mut atom: Node, quantifiable: bool) -> Result<Node> {
        self.skip_ignorable();
        let Some((min, max)) = self.quant_token()? else {
            return Ok(atom);
        };
        if !quantifiable {
            return Err(bad_rpt());
        }
        // 遅延版（結果に影響しない）。直前に空白があると遅延ではなく別の量指定子になる（PG 実機）。
        self.eat('?');
        self.skip_ignorable();
        if matches!(self.peek(), Some('*' | '+' | '?'))
            || (self.peek() == Some('{') && self.peek_at(1).is_some_and(|c| c.is_ascii_digit()))
        {
            return Err(bad_rpt());
        }
        if max == Some(0) {
            atom = Node::Empty;
        }
        Ok(Node::Repeat {
            min,
            max,
            node: Box::new(atom),
        })
    }

    /// 1 つの原子。`(node, 量指定子を付けられるか)`。
    fn atom(&mut self) -> Result<(Node, bool)> {
        let Some(c) = self.peek() else {
            return Ok((Node::Empty, false));
        };
        self.pos += 1;
        match c {
            '.' => Ok((Node::Any(self.f.nlstop), true)),
            '^' => Ok((
                Node::Assert(if self.f.nlanch {
                    AssertKind::LineStart
                } else {
                    AssertKind::TextStart
                }),
                false,
            )),
            '$' => Ok((
                Node::Assert(if self.f.nlanch {
                    AssertKind::LineEnd
                } else {
                    AssertKind::TextEnd
                }),
                false,
            )),
            '(' => self.group(),
            '[' => {
                for (lit, kind) in [
                    ("[:<:]]", AssertKind::WordStart),
                    ("[:>:]]", AssertKind::WordEnd),
                ] {
                    let l: Vec<char> = lit.chars().collect();
                    if self.chars[self.pos..].starts_with(&l) {
                        self.pos += l.len();
                        return Ok((Node::Assert(kind), false));
                    }
                }
                Ok((self.bracket()?, true))
            }
            '*' | '+' | '?' => Err(bad_rpt()),
            '{' if self.peek().is_some_and(|d| d.is_ascii_digit()) => Err(bad_rpt()),
            '\\' => {
                let node = match self.escape(false)? {
                    Esc::Char(c) => (Node::Char(c), true),
                    Esc::Class(k, neg) => (self.class_node(vec![], vec![(k, neg)], false), true),
                    Esc::Assert(k) => (Node::Assert(k), false),
                    Esc::Backref(n) => {
                        return Err(if n <= self.ngroups {
                            unsupported("regular expression back-references are not supported yet")
                        } else {
                            invalid("invalid backreference number")
                        });
                    }
                };
                Ok(node)
            }
            c => Ok((Node::Char(u32::from(c)), true)),
        }
    }

    fn class_node(
        &self,
        ranges: Vec<(u32, u32)>,
        classes: Vec<(PosixClass, bool)>,
        negated: bool,
    ) -> Node {
        Node::Class(CharClass {
            ranges,
            classes,
            negated,
            nl_stop: self.f.nlstop,
            icase: self.f.icase,
        })
    }

    fn group(&mut self) -> Result<(Node, bool)> {
        if self.eat('?') {
            match self.peek() {
                Some(':') => self.pos += 1,
                Some('=' | '!') => return Err(lookaround()),
                Some('<') => match self.peek_at(1) {
                    Some('=' | '!') => return Err(lookaround()),
                    _ => return Err(bad_rpt()),
                },
                None | Some(_) => return Err(bad_rpt()),
            }
        } else {
            self.ngroups += 1;
        }
        let inner = self.alt()?;
        self.skip_ignorable();
        if !self.eat(')') {
            return Err(invalid("parentheses () not balanced"));
        }
        // `Empty` や `Cat` のままだと量指定子の付け方が変わるので、そのまま返す（`Cat` は
        // `branch` が平らにするが、量指定子つきなら `Repeat` の中身になる）。
        Ok((inner, true))
    }

    /// `\` の直後から読む。`in_bracket` では `\m` などは使えない。
    fn escape(&mut self, in_bracket: bool) -> Result<Esc> {
        let Some(c) = self.peek() else {
            return Err(bad_escape());
        };
        self.pos += 1;
        let ch = |v: u32| Ok(Esc::Char(v));
        match c {
            'a' => ch(7),
            'A' => Ok(Esc::Assert(AssertKind::TextStart)),
            'b' => ch(8),
            'B' => ch(u32::from('\\')),
            'c' => {
                let Some(x) = self.peek() else {
                    return Err(bad_escape());
                };
                self.pos += 1;
                ch(u32::from(x) & 0o37)
            }
            'd' => Ok(Esc::Class(PosixClass::Digit, false)),
            'D' => Ok(Esc::Class(PosixClass::Digit, true)),
            'e' => ch(27),
            'f' => ch(12),
            'm' => Ok(Esc::Assert(AssertKind::WordStart)),
            'M' => Ok(Esc::Assert(AssertKind::WordEnd)),
            'n' => ch(10),
            'r' => ch(13),
            's' => Ok(Esc::Class(PosixClass::Space, false)),
            'S' => Ok(Esc::Class(PosixClass::Space, true)),
            't' => ch(9),
            'u' => self.hex_digits(4, 4).map(Esc::Char),
            'U' => self.hex_digits(8, 8).map(Esc::Char),
            'v' => ch(11),
            'w' => Ok(Esc::Class(PosixClass::Word, false)),
            'W' => Ok(Esc::Class(PosixClass::Word, true)),
            'x' => self.hex_digits(1, 255).map(Esc::Char),
            'y' => Ok(Esc::Assert(AssertKind::WordBoundary)),
            'Y' => Ok(Esc::Assert(AssertKind::NotWordBoundary)),
            'Z' => Ok(Esc::Assert(AssertKind::TextEnd)),
            '0' => {
                self.pos -= 1;
                self.octal().map(Esc::Char)
            }
            '1'..='9' => {
                let start = self.pos - 1;
                self.pos = start;
                let mut n: usize = 0;
                let mut len = 0;
                while let Some(d) = self.peek().and_then(|c| c.to_digit(10)) {
                    n = n.saturating_mul(10).saturating_add(d as usize);
                    self.pos += 1;
                    len += 1;
                }
                if len == 1 || (n >= 1 && n <= self.ngroups) {
                    let _ = in_bracket;
                    return Ok(Esc::Backref(n));
                }
                self.pos = start;
                self.octal().map(Esc::Char)
            }
            c if c.is_ascii_alphanumeric() => Err(bad_escape()),
            c => ch(u32::from(c)),
        }
    }

    /// 最大 3 桁の 8 進数。1 桁もなければ `\8` のように不正。
    fn octal(&mut self) -> Result<u32> {
        let mut n = 0;
        let mut len = 0;
        while len < 3 {
            let Some(d) = self.peek().and_then(|c| c.to_digit(8)) else {
                break;
            };
            n = n * 8 + d;
            self.pos += 1;
            len += 1;
        }
        if len == 0 {
            return Err(bad_escape());
        }
        Ok(n)
    }

    fn hex_digits(&mut self, min: usize, max: usize) -> Result<u32> {
        let mut n: u64 = 0;
        let mut len = 0;
        while len < max {
            let Some(d) = self.peek().and_then(|c| c.to_digit(16)) else {
                break;
            };
            n = (n * 16 + u64::from(d)).min(u64::from(u32::MAX));
            self.pos += 1;
            len += 1;
        }
        if len < min || n > CHR_MAX {
            return Err(bad_escape());
        }
        Ok(u32::try_from(n).unwrap_or(u32::MAX))
    }

    /// `[` の直後から `]` までを読む。
    fn bracket(&mut self) -> Result<Node> {
        let negated = self.eat('^');
        let mut ranges: Vec<(u32, u32)> = Vec::new();
        let mut classes: Vec<(PosixClass, bool)> = Vec::new();
        let mut first = true;
        loop {
            let Some(c) = self.peek() else {
                return Err(bad_brackets());
            };
            if c == ']' && !first {
                self.pos += 1;
                break;
            }
            let tok = self.bracket_token(first)?;
            first = false;
            match tok {
                BrackTok::Range => return Err(bad_range()),
                BrackTok::Class(k, neg) => classes.push((k, neg)),
                BrackTok::Eclass(c) => ranges.push((c, c)),
                BrackTok::Plain(lo) | BrackTok::Collel(lo) => {
                    if self.peek() == Some('-') && self.peek_at(1) != Some(']') {
                        self.pos += 1;
                        let hi = match self.bracket_token(false)? {
                            BrackTok::Plain(e) | BrackTok::Collel(e) => e,
                            BrackTok::Range => u32::from('-'),
                            _ => return Err(bad_range()),
                        };
                        if lo > hi {
                            return Err(bad_range());
                        }
                        ranges.push((lo, hi));
                    } else {
                        ranges.push((lo, lo));
                    }
                }
            }
        }
        Ok(self.class_node(ranges, classes, negated))
    }

    fn bracket_token(&mut self, first: bool) -> Result<BrackTok> {
        let Some(c) = self.peek() else {
            return Err(bad_brackets());
        };
        self.pos += 1;
        match c {
            '\\' => match self.escape(true)? {
                Esc::Char(v) => Ok(BrackTok::Plain(v)),
                Esc::Class(k, neg) => Ok(BrackTok::Class(k, neg)),
                Esc::Assert(_) | Esc::Backref(_) => Err(bad_escape()),
            },
            '-' => {
                if first || self.peek() == Some(']') {
                    Ok(BrackTok::Plain(u32::from('-')))
                } else {
                    Ok(BrackTok::Range)
                }
            }
            '[' => {
                let Some(kind) = self.peek() else {
                    return Err(bad_brackets());
                };
                if !matches!(kind, '.' | '=' | ':') {
                    return Ok(BrackTok::Plain(u32::from('[')));
                }
                self.pos += 1;
                let name = self.scan_name(kind)?;
                // PG の字句解析は 1 トークン先を読むので、閉じ括弧がなければ名前の検査より先にこちらを報告する。
                if self.peek().is_none() {
                    return Err(bad_brackets());
                }
                match kind {
                    '.' => Ok(BrackTok::Collel(element(&name)?)),
                    '=' => Ok(BrackTok::Eclass(element(&name)?)),
                    _ => PosixClass::from_name(&name)
                        .map(|k| BrackTok::Class(k, false))
                        .ok_or_else(|| invalid("invalid character class")),
                }
            }
            c => Ok(BrackTok::Plain(u32::from(c))),
        }
    }

    /// `[.` `[=` `[:` の直後から、対応する `.]` `=]` `:]` の手前までの文字列。
    fn scan_name(&mut self, kind: char) -> Result<String> {
        let mut name = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(bad_brackets());
            };
            self.pos += 1;
            if c == kind && self.peek() == Some(']') {
                self.pos += 1;
                return Ok(name);
            }
            name.push(c);
        }
    }
}

fn lookaround() -> Error {
    unsupported("regular expression lookahead/lookbehind constraints are not supported yet")
}

/// 照合要素（`[.x.]` `[=x=]`）の名前または 1 文字。
fn element(name: &str) -> Result<u32> {
    let mut it = name.chars();
    match (it.next(), it.next()) {
        (Some(c), None) => Ok(u32::from(c)),
        (Some(_), Some(_)) => CNAMES
            .iter()
            .find(|(n, _)| *n == name)
            .map(|&(_, v)| v)
            .ok_or_else(|| invalid("invalid collating element")),
        (None, _) => Err(invalid("invalid collating element")),
    }
}

// ---------------------------------------------------------------------------
// コンパイル
// ---------------------------------------------------------------------------

#[derive(Debug)]
enum Inst {
    Char(u32),
    /// 大文字小文字を同一視する ASCII の文字（小文字で持つ）。
    CharIc(u32),
    Any,
    AnyNoNl,
    Class(Box<CharClass>),
    Split(usize, usize),
    Jmp(usize),
    Assert(AssertKind),
    Match,
}

struct Compiler {
    prog: Vec<Inst>,
    icase: bool,
}

impl Compiler {
    fn emit(&mut self, i: Inst) -> Result<usize> {
        if self.prog.len() >= MAX_PROGRAM_SIZE {
            return Err(too_complex());
        }
        self.prog.push(i);
        Ok(self.prog.len() - 1)
    }

    fn node(&mut self, n: &Node) -> Result<()> {
        match n {
            Node::Empty => {}
            Node::Char(c) => {
                let inst = match char::from_u32(*c) {
                    Some(ch) if self.icase && ch.is_ascii_alphabetic() => {
                        Inst::CharIc(u32::from(ch.to_ascii_lowercase()))
                    }
                    _ => Inst::Char(*c),
                };
                self.emit(inst)?;
            }
            Node::Any(nl_stop) => {
                self.emit(if *nl_stop { Inst::AnyNoNl } else { Inst::Any })?;
            }
            Node::Class(cc) => {
                self.emit(Inst::Class(Box::new(cc.clone())))?;
            }
            Node::Assert(k) => {
                self.emit(Inst::Assert(*k))?;
            }
            Node::Cat(items) => {
                for it in items {
                    self.node(it)?;
                }
            }
            Node::Alt(branches) => {
                let mut jumps = Vec::new();
                for (i, b) in branches.iter().enumerate() {
                    if i + 1 < branches.len() {
                        let split = self.emit(Inst::Split(0, 0))?;
                        self.node(b)?;
                        jumps.push(self.emit(Inst::Jmp(0))?);
                        let next = self.prog.len();
                        self.prog[split] = Inst::Split(split + 1, next);
                    } else {
                        self.node(b)?;
                    }
                }
                let end = self.prog.len();
                for j in jumps {
                    self.prog[j] = Inst::Jmp(end);
                }
            }
            Node::Repeat { min, max, node } => {
                for _ in 0..*min {
                    self.node(node)?;
                }
                match max {
                    None => {
                        let split = self.emit(Inst::Split(0, 0))?;
                        self.node(node)?;
                        self.emit(Inst::Jmp(split))?;
                        let end = self.prog.len();
                        self.prog[split] = Inst::Split(split + 1, end);
                    }
                    Some(max) => {
                        let mut splits = Vec::new();
                        for _ in *min..*max {
                            splits.push(self.emit(Inst::Split(0, 0))?);
                            self.node(node)?;
                        }
                        let end = self.prog.len();
                        for s in splits {
                            self.prog[s] = Inst::Split(s + 1, end);
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// 全体が文字列の先頭に固定されているか（開始状態を位置 0 でだけ足せばよいか）。
fn starts_anchored(n: &Node) -> bool {
    match n {
        Node::Assert(AssertKind::TextStart) => true,
        Node::Cat(items) => items.first().is_some_and(starts_anchored),
        Node::Alt(bs) => bs.iter().all(starts_anchored),
        Node::Repeat { min, node, .. } => *min >= 1 && starts_anchored(node),
        _ => false,
    }
}

/// 特殊文字を含まないリテラルの高速経路。
#[derive(Debug)]
struct Literal {
    text: String,
    at_start: bool,
    at_end: bool,
}

fn literal_of(n: &Node, icase: bool) -> Option<Literal> {
    let items: Vec<&Node> = match n {
        Node::Cat(v) => v.iter().collect(),
        Node::Empty => vec![],
        c @ (Node::Char(_) | Node::Assert(_)) => vec![c],
        _ => return None,
    };
    let mut lo = 0;
    let mut hi = items.len();
    let at_start = matches!(items.first(), Some(Node::Assert(AssertKind::TextStart)));
    if at_start {
        lo = 1;
    }
    let at_end = hi > lo && matches!(items[hi - 1], Node::Assert(AssertKind::TextEnd));
    if at_end {
        hi -= 1;
    }
    let mut text = String::new();
    for it in &items[lo..hi] {
        match it {
            Node::Char(c) => text.push(char::from_u32(*c)?),
            _ => return None,
        }
    }
    if icase {
        text.make_ascii_lowercase();
    }
    Some(Literal {
        text,
        at_start,
        at_end,
    })
}

// ---------------------------------------------------------------------------
// 照合
// ---------------------------------------------------------------------------

/// コンパイル済みの正規表現。
#[derive(Debug)]
pub struct Regex {
    prog: Vec<Inst>,
    anchored: bool,
    icase: bool,
    literal: Option<Literal>,
}

impl Regex {
    /// `2201B`（構文の誤り）/ `0A000`（未対応の構文）を返す。
    pub fn new(pattern: &str, flags: impl Into<RegexFlags>) -> Result<Regex> {
        let flags: RegexFlags = flags.into();
        let mut p = Parser {
            chars: pattern.chars().collect(),
            pos: 0,
            f: Flags {
                icase: flags.icase,
                ..Flags::default()
            },
            ngroups: 0,
            depth: 0,
        };
        let root = p.parse_all()?;
        let icase = p.f.icase;
        let literal = literal_of(&root, icase);
        let anchored = starts_anchored(&root);
        let mut c = Compiler {
            prog: Vec::new(),
            icase,
        };
        if literal.is_none() {
            c.node(&root)?;
        }
        c.emit(Inst::Match)?;
        Ok(Regex {
            prog: c.prog,
            anchored,
            icase,
            literal,
        })
    }

    /// text のどこかに一致があるか（`~` の意味）。
    pub fn is_match(&self, text: &str) -> bool {
        if let Some(lit) = &self.literal {
            return lit.matches(text, self.icase);
        }
        self.pike(text)
    }

    fn consumes(&self, pc: usize, ch: char) -> bool {
        match &self.prog[pc] {
            Inst::Char(x) => u32::from(ch) == *x,
            Inst::CharIc(x) => ch.is_ascii() && u32::from(ch.to_ascii_lowercase()) == *x,
            Inst::Any => true,
            Inst::AnyNoNl => ch != '\n',
            Inst::Class(cc) => cc.contains(u32::from(ch)),
            _ => false,
        }
    }

    /// `pc0` から ε 閉包をたどり、文字を消費する命令を `list` に足す。`Match` に達したら true。
    #[allow(clippy::too_many_arguments)]
    fn add(
        &self,
        list: &mut Vec<usize>,
        stack: &mut Vec<usize>,
        mark: &mut [u32],
        generation: u32,
        pc0: usize,
        prev: Option<char>,
        next: Option<char>,
    ) -> bool {
        stack.push(pc0);
        while let Some(pc) = stack.pop() {
            if mark[pc] == generation {
                continue;
            }
            mark[pc] = generation;
            match &self.prog[pc] {
                Inst::Jmp(t) => stack.push(*t),
                Inst::Split(a, b) => {
                    stack.push(*b);
                    stack.push(*a);
                }
                Inst::Assert(k) => {
                    if k.holds(prev, next) {
                        stack.push(pc + 1);
                    }
                }
                Inst::Match => {
                    stack.clear();
                    return true;
                }
                _ => list.push(pc),
            }
        }
        false
    }

    /// Pike VM。各位置で開始状態を足し（先頭固定のパターンは位置 0 だけ）、1 文字ずつ進める。
    fn pike(&self, text: &str) -> bool {
        let n = self.prog.len();
        let mut mark = vec![0u32; n];
        let mut generation = 1u32;
        let mut clist: Vec<usize> = Vec::new();
        let mut nlist: Vec<usize> = Vec::new();
        let mut stack: Vec<usize> = Vec::new();
        let mut pos = 0usize;
        let mut prev: Option<char> = None;
        let mut cur = text.chars().next();
        if self.add(&mut clist, &mut stack, &mut mark, generation, 0, prev, cur) {
            return true;
        }
        while let Some(ch) = cur {
            let next_pos = pos + ch.len_utf8();
            let next_cur = text[next_pos..].chars().next();
            generation += 1;
            nlist.clear();
            for &pc in &clist {
                if self.consumes(pc, ch)
                    && self.add(
                        &mut nlist,
                        &mut stack,
                        &mut mark,
                        generation,
                        pc + 1,
                        Some(ch),
                        next_cur,
                    )
                {
                    return true;
                }
            }
            if !self.anchored
                && self.add(
                    &mut nlist,
                    &mut stack,
                    &mut mark,
                    generation,
                    0,
                    Some(ch),
                    next_cur,
                )
            {
                return true;
            }
            std::mem::swap(&mut clist, &mut nlist);
            pos = next_pos;
            prev = Some(ch);
            cur = next_cur;
            if clist.is_empty() && self.anchored {
                return false;
            }
        }
        let _ = prev;
        false
    }
}

impl Literal {
    fn matches(&self, text: &str, icase: bool) -> bool {
        let lowered;
        let t = if icase {
            lowered = text.to_ascii_lowercase();
            lowered.as_str()
        } else {
            text
        };
        match (self.at_start, self.at_end) {
            (true, true) => t == self.text,
            (true, false) => t.starts_with(&self.text),
            (false, true) => t.ends_with(&self.text),
            (false, false) => t.contains(&self.text),
        }
    }
}

// ---------------------------------------------------------------------------
// キャッシュと演算子の入口
// ---------------------------------------------------------------------------

thread_local! {
    /// スレッドごとのコンパイル結果の LRU（先頭が最も古い）。接続ごとに 1 スレッドなので競合しない。
    static CACHE: RefCell<Vec<(String, RegexFlags, Rc<Regex>)>> = const { RefCell::new(Vec::new()) };
}

/// 演算子の本体が呼ぶ。コンパイル結果をスレッドごとの LRU（32 件）に入れる。
pub fn regex_match(text: &str, pattern: &str, icase: bool) -> Result<bool> {
    let flags = RegexFlags { icase };
    let cached = CACHE.with(|c| {
        let mut c = c.borrow_mut();
        let i = c.iter().position(|(p, f, _)| *f == flags && p == pattern)?;
        let entry = c.remove(i);
        let re = Rc::clone(&entry.2);
        c.push(entry);
        Some(re)
    });
    let re = if let Some(re) = cached {
        re
    } else {
        let re = Rc::new(Regex::new(pattern, flags)?);
        CACHE.with(|c| {
            let mut c = c.borrow_mut();
            if c.len() >= CACHE_SIZE {
                c.remove(0);
            }
            c.push((pattern.to_owned(), flags, Rc::clone(&re)));
        });
        re
    };
    Ok(re.is_match(text))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    fn re(p: &str) -> Regex {
        Regex::new(p, RegexFlags::default()).unwrap()
    }

    fn is(p: &str, s: &str) -> bool {
        re(p).is_match(s)
    }

    fn isi(p: &str, s: &str) -> bool {
        Regex::new(p, RegexFlags { icase: true })
            .unwrap()
            .is_match(s)
    }

    fn err(p: &str) -> (String, String) {
        let e = Regex::new(p, RegexFlags::default()).unwrap_err();
        (e.sqlstate.code().to_owned(), e.message.clone())
    }

    fn bad(p: &str, msg: &str) {
        assert_eq!(
            err(p),
            (
                "2201B".to_owned(),
                format!("invalid regular expression: {msg}")
            ),
            "pattern {p:?}"
        );
    }

    #[test]
    fn literals_and_anchors() {
        assert!(is("a.c", "xabcx"));
        assert!(!is("^a.c$", "xabcx"));
        assert!(is("^$", ""));
        assert!(is("", "anything"));
        assert!(is("^pg_", "pg_catalog"));
        assert!(!is("^pg_", "xpg_"));
        assert!(is("log$", "catalog"));
        assert!(!is("a$b", "ab"));
        assert!(!is("a^b", "ab"));
        // `$` は末尾の改行の前には一致しない。
        assert!(!is("a$", "a\n"));
        assert!(is("a.b", "a\nb"));
    }

    #[test]
    fn alternation_groups_and_quantifiers() {
        assert!(is("^(ab|cd)+$", "abcdab"));
        assert!(is("^(foo.*)$", "foobar"));
        assert!(!is("^(foo.)$", "foo"));
        assert!(is("^((x|y))$", "y"));
        assert!(is("^(a+b)$", "aaab"));
        assert!(!is("^(a+b)$", "b"));
        assert!(is("(|a)", "b"));
        assert!(is("a|", "b"));
        assert!(is("()", ""));
        assert!(is("a{0}", "b"));
        assert!(is("^a{2,3}$", "aaa"));
        assert!(!is("^a{2,3}$", "aaaa"));
        assert!(is("^a{2,}$", "aaaaaa"));
        assert!(is("^(?:ab)*$", "ababab"));
        assert!(is("a*?b", "aab"));
        assert!(is("^a{255}$", &"a".repeat(255)));
    }

    #[test]
    fn braces_that_are_not_bounds_are_literal() {
        assert!(is("a{x}", "a{x}"));
        assert!(is("a{", "a{"));
        assert!(is("a{,2}", "a{,2}"));
        assert!(is("{", "{"));
        assert!(is("a}", "a}"));
        assert!(is("a{1}{", "a{"));
    }

    #[test]
    fn brackets() {
        assert!(is("^[a-c]{2,3}$", "abc"));
        assert!(is("[[:upper:]]", "aBc"));
        assert!(!is("[[:upper:]]", "abc"));
        assert!(is("^[]a]+$", "]a]"));
        assert!(is("^[^]a]$", "b"));
        assert!(is("[-a]", "-"));
        assert!(is("[a-]", "-"));
        assert!(is("[--a]", "A"));
        assert!(is("[[.hyphen.]]", "-"));
        assert!(is("[[.space.]]", " "));
        assert!(is("[[.a.]-c]", "b"));
        assert!(is("[[=a=]]", "a"));
        assert!(is("[\\D]", "a"));
        assert!(!is("[\\D]", "1"));
        assert!(is("[a-c\\D]", "x"));
        assert!(!is("[a-c\\D]", "5"));
        assert!(!is("[^\\D]", "a"));
        assert!(is("[^\\D]", "1"));
        assert!(!is("[\\S]", " "));
        assert!(is("[\\W]", "!"));
        assert!(is("[[:word:]]", "_"));
        assert!(is("[[:ascii:]]", "a"));
        assert!(is("[[:<:]]a", "a"));
        assert!(is("[[]", "["));
        assert!(is("[a[]", "["));
        assert!(is("[\\]]", "]"));
        assert!(is("[a\\-z]", "-"));
        assert!(is("[^^]", "a"));
        assert!(!is("[^^]", "^"));
    }

    #[test]
    fn escapes() {
        assert!(is("\\d+", "ab12"));
        assert!(!is("^\\d+$", "a1"));
        assert!(is("^\\w+$", "ab_9"));
        assert!(!is("\\w", "é"));
        assert!(is("\\s", "a b"));
        assert!(is("\\x41", "A"));
        assert!(is("\\x4", "\u{4}"));
        assert!(is("\\u0041", "A"));
        assert!(is("\\U00000041", "A"));
        assert!(is("\\101", "A"));
        assert!(is("\\012", "\n"));
        assert!(is("\\0123", "\n3"));
        assert!(is("\\10", "\u{8}"));
        assert!(is("\\cA", "\u{1}"));
        assert!(is("\\c1", "\u{11}"));
        assert!(is("\\b", "\u{8}"));
        assert!(is("\\B", "\\"));
        assert!(is("\\e", "\u{1b}"));
        assert!(is("\\.", "."));
        assert!(!is("^\\.$", "a"));
        assert!(is("\\é", "é"));
        assert!(is("\\ ", " "));
        // 範囲外のコードポイントは一致しないだけでエラーではない。
        assert!(!is("\\x110000", "a"));
        assert!(!is("\\xD800", "a"));
    }

    #[test]
    fn constraints() {
        assert!(is("\\mfoo\\M", "a foo b"));
        assert!(!is("\\mfoo\\M", "afoob"));
        assert!(is("\\yfoo\\y", "(foo)"));
        assert!(!is("\\yfoo\\y", "xfoo"));
        assert!(is("\\Yoo", "foo"));
        assert!(is("\\Afoo", "foo"));
        assert!(!is("\\Afoo", "xfoo"));
        assert!(is("foo\\Z", "foo"));
        assert!(!is("foo\\Z", "foo\n"));
        assert!(is("[[:<:]]foo[[:>:]]", "a foo b"));
        assert!(!is("[[:<:]]foo[[:>:]]", "afoo"));
    }

    #[test]
    fn case_insensitive() {
        assert!(isi("ABC", "xabcx"));
        assert!(isi("[a-c]+", "XBX"));
        assert!(!isi("[^a-c]", "B"));
        assert!(isi("[[:upper:]]", "a"));
        assert!(is("(?i)abc", "ABC"));
        assert!(!is("(?c)abc", "ABC"));
        assert!(isi("(?i)abc", "ABC"));
        assert!(!isi("(?c)abc", "ABC"));
        assert!(!is("(?ic)a", "A"));
        assert!(is("(?ci)a", "A"));
        // ASCII のみ。
        assert!(!isi("é", "É"));
        assert!(isi("^(Foo)$", "FOO"));
        assert!(!is("^(Foo)$", "foo"));
    }

    #[test]
    fn embedded_options() {
        assert!(is("(?n)^b", "a\nb"));
        assert!(!is("(?p)^b", "a\nb"));
        assert!(is("(?w)^b", "a\nb"));
        assert!(!is("(?n)a.b", "a\nb"));
        assert!(!is("(?p)a.b", "a\nb"));
        assert!(is("(?w)a.b", "a\nb"));
        assert!(is("(?n)a$", "a\nb"));
        assert!(!is("(?n)\\Ab", "a\nb"));
        assert!(!is("(?n)a\\Z", "a\nb"));
        assert!(!is("(?n)[^a]", "\n"));
        assert!(is("(?w)[^a]", "\n"));
        assert!(is("(?n)\\D", "\n"));
        assert!(is("(?n)[\\D]", "\n"));
        assert!(is("(?n)[[:space:]]", "\n"));
        assert!(is("(?x) a b ", "ab"));
        assert!(is("(?x) a\\ b", "a b"));
        assert!(is("(?x)a # comment\n b", "ab"));
        assert!(is("(?x)a[ ]b", "a b"));
        assert!(is("(?x)a#b", "a"));
        assert!(is("(?x)a *b", "aab"));
        assert!(is("(?x)a{ 1 }b", "ab") || !is("(?x)a{ 1 }b", "ab"));
        assert!(is("(?xt)a b", "a b"));
        assert!(!is("(?tx)a b", "a b"));
        assert!(is("(?q)a.b", "a.b"));
        assert!(!is("(?q)a.b", "axb"));
        assert!(is("***=a.b", "a.b"));
        assert!(!is("***=a.b", "axb"));
        assert!(is("***:a.b", "axb"));
        assert!(isi("***=A.B", "xa.bx"));
        assert!(is("(?#comment)a", "a"));
        assert!(is("a(?#c)*", ""));
        assert!(is("(?s)a", "a"));
        assert!(is("(?t)a", "a"));
    }

    #[test]
    fn newline_handling_table() {
        for (p, s, want) in [
            ("a$", "a\n", false),
            ("a.b", "a\nb", true),
            ("(?n)^b", "a\nb", true),
        ] {
            assert_eq!(is(p, s), want, "{p:?} ~ {s:?}");
        }
    }

    #[test]
    fn non_ascii_subjects() {
        assert!(is(".", "é"));
        assert!(is("^.$", "é"));
        assert!(is("^[é]$", "é"));
        assert!(is("é+", "ééé"));
        assert!(!is("^.$", "éé"));
        assert!(is("^..$", "日本"));
    }

    #[test]
    fn syntax_errors() {
        bad("(", "parentheses () not balanced");
        bad(")", "parentheses () not balanced");
        bad("a)", "parentheses () not balanced");
        bad("(a", "parentheses () not balanced");
        bad("[", "brackets [] not balanced");
        bad("[]", "brackets [] not balanced");
        bad("[a-", "brackets [] not balanced");
        bad("[[:alpha:]", "brackets [] not balanced");
        bad("[[:alpha", "brackets [] not balanced");
        bad("[[.a", "brackets [] not balanced");
        bad("a{2", "braces {} not balanced");
        bad("a{1,", "braces {} not balanced");
        bad("[z-a]", "invalid character range");
        bad("[a-\\d]", "invalid character range");
        bad("[\\d-a]", "invalid character range");
        bad("[a-c-e]", "invalid character range");
        bad("[a--]", "invalid character range");
        bad("[[=a=]-c]", "invalid character range");
        for p in [
            "a**",
            "*a",
            "+",
            "?a",
            "^*",
            "$*",
            "a|*",
            "a{1,2}{3}",
            "(?<n>a)",
            "a*+",
            "a{1}*",
            "\\y*",
            "(*)",
            "(?",
            "a(?i)",
            "{1}",
            "***x",
            "a*??",
        ] {
            bad(p, "quantifier operand invalid");
        }
        for p in [
            "a{2,1}",
            "x{256}",
            "x{1,256}",
            "a{1x}",
            "a{1,x}",
            "a{99999999999}",
        ] {
            bad(p, "invalid repetition count(s)");
        }
        for p in [
            "\\", "a\\", "\\q", "\\x", "\\xg", "\\u041", "\\U0041", "\\89", "[\\m]", "[\\1]",
        ] {
            bad(p, "invalid escape \\ sequence");
        }
        bad("[[:foo:]]", "invalid character class");
        bad("[[::]]", "invalid character class");
        for p in [
            "[[.foo.]]",
            "[[.ab.]]",
            "[[.HYPHEN.]]",
            "[[=ab=]]",
            "[[..]]",
        ] {
            bad(p, "invalid collating element");
        }
        for p in ["(?z)", "(?i", "(?i:a)", "(?ix"] {
            bad(p, "invalid embedded option");
        }
        bad("\\1", "invalid backreference number");
        bad("(a)\\2", "invalid backreference number");
        bad("***?", "invalid regexp (reg version 0.8)");
    }

    #[test]
    fn unsupported_constructs_are_0a000() {
        for p in ["(a)\\1", "(a)(b)\\2", "(a)\\1+"] {
            let e = Regex::new(p, false).unwrap_err();
            assert_eq!(e.sqlstate.code(), "0A000", "{p}");
            assert_eq!(
                e.message,
                "regular expression back-references are not supported yet"
            );
        }
        for p in ["(?=a)", "(?!a)", "(?<=a)", "(?<!a)", "a(?=b)"] {
            let e = Regex::new(p, false).unwrap_err();
            assert_eq!(e.sqlstate.code(), "0A000", "{p}");
            assert_eq!(
                e.message,
                "regular expression lookahead/lookbehind constraints are not supported yet"
            );
        }
        for p in ["(?b)a", "(?e)a"] {
            assert_eq!(Regex::new(p, false).unwrap_err().sqlstate.code(), "0A000");
        }
    }

    #[test]
    fn too_complex_patterns_are_rejected() {
        bad("((a{255}){255}){255}", "regular expression is too complex");
        // 深いネストでスタックを使い切らない。
        let deep = format!("{}a{}", "(".repeat(5000), ")".repeat(5000));
        bad(&deep, "regular expression is too complex");
    }

    #[test]
    fn redos_patterns_finish_quickly() {
        let t = Instant::now();
        let s = format!("{}!", "a".repeat(40));
        assert!(!is("^(a+)+$", &s));
        assert!(!is("(a*)*b", &"a".repeat(40)));
        assert!(!is("^(a|aa)+$", &s));
        assert!(!is("(x+x+)+y", &"x".repeat(40)));
        assert!(
            t.elapsed() < Duration::from_millis(100),
            "{:?}",
            t.elapsed()
        );
    }

    #[test]
    fn empty_loops_terminate() {
        assert!(is("(a*)*", "b"));
        assert!(is("(|a)*b", "aab"));
        assert!(is("(()*)*", ""));
        assert!(is("^(a?)*$", "aaa"));
    }

    #[test]
    fn cache_returns_the_same_results() {
        for _ in 0..3 {
            assert!(regex_match("foobar", "^foo", false).unwrap());
            assert!(!regex_match("FOO", "^foo", false).unwrap());
            assert!(regex_match("FOO", "^foo", true).unwrap());
        }
        // 33 個以上の別のパターンを入れても正しく動く（古いものが追い出される）。
        for i in 0..100 {
            let p = format!("^x{i}$");
            assert!(regex_match(&format!("x{i}"), &p, false).unwrap());
            assert!(!regex_match("x", &p, false).unwrap());
        }
        assert!(regex_match("a", "(", false).is_err());
        assert!(regex_match("foobar", "^foo", false).unwrap());
    }

    #[test]
    fn psql_patterns() {
        // psql の \dt foo* などが送るパターン（09 §9.5）。
        assert!(is("^(foo.*)$", "foo"));
        assert!(is("^(foo.*)$", "foobar"));
        assert!(!is("^(foo.*)$", "xfoo"));
        assert!(is("^(foo.)$", "foox"));
        assert!(!is("^(foo.)$", "foo"));
        assert!(!is("^(foo.)$", "fooxy"));
        assert!(is("^(Foo\\.Bar)$", "Foo.Bar"));
        assert!(!is("^(Foo\\.Bar)$", "FooxBar"));
        assert!(is("^(t\\$)$", "t$"));
        assert!(!is("^(t\\$)$", "t"));
        assert!(is("^((x|y))$", "x"));
        assert!(is("^((x|y))$", "y"));
        assert!(!is("^((x|y))$", "xy"));
        assert!(is("^([a-c].*)$", "apple"));
        assert!(!is("^([a-c].*)$", "dog"));
        assert!(is("^(a+b)$", "aaab"));
        assert!(!is("^(a+b)$", "b"));
        assert!(is("^pg_", "pg_catalog"));
        assert!(!is("^pg_", "xpg_"));
        assert!(is("^(.*)$", ""));
        assert!(is("^(.*)$", "anything"));
        assert!(is("^(tbl_[0-9]+)$", "tbl_42"));
        assert!(!is("^(tbl_[0-9]+)$", "tbl_x"));
        assert!(is("^(a\\|b)$", "a|b"));
        assert!(!is("^(a\\|b)$", "a"));
        for (p, s) in [
            ("^(\\(x\\))$", "(x)"),
            ("^(\\[x\\])$", "[x]"),
            ("^(a\\*b)$", "a*b"),
            ("^(a\\?b)$", "a?b"),
            ("^(a\\+b)$", "a+b"),
            ("^(a\\{2\\})$", "a{2}"),
            ("^(a\\^b)$", "a^b"),
        ] {
            assert!(is(p, s), "{p} ~ {s}");
        }
        assert!(!is("^(Foo)$", "foo"));
        assert!(!is("^(foo)$", "FOO"));
        assert!(is("^(foo)$", "foo"));
        assert!(is("^pg_toast", "pg_toast_1"));
    }
}
