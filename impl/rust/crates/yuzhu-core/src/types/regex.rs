//! A small backtracking regular expression engine for the `~` family of
//! operators. It covers the common subset of PostgreSQL's advanced regular
//! expressions: literals, `.`, bracket expressions (ranges, POSIX classes,
//! `\d \w \s` escapes), anchors, groups (capturing and `(?:...)`),
//! alternation and the quantifiers `* + ? {m,n}` with lazy variants.

use crate::error::{Error, Result, SqlState};

#[derive(Debug, Clone, Copy)]
enum Class {
    Digit,
    Word,
    Space,
    Alpha,
    Alnum,
    Upper,
    Lower,
    Punct,
    Xdigit,
    Blank,
    Cntrl,
    Graph,
    Print,
}

impl Class {
    fn matches(self, c: char) -> bool {
        match self {
            Class::Digit => c.is_ascii_digit(),
            Class::Word => c.is_ascii_alphanumeric() || c == '_',
            Class::Space => matches!(c, ' ' | '\t' | '\n' | '\r' | '\x0b' | '\x0c'),
            Class::Alpha => c.is_ascii_alphabetic(),
            Class::Alnum => c.is_ascii_alphanumeric(),
            Class::Upper => c.is_ascii_uppercase(),
            Class::Lower => c.is_ascii_lowercase(),
            Class::Punct => c.is_ascii_punctuation(),
            Class::Xdigit => c.is_ascii_hexdigit(),
            Class::Blank => matches!(c, ' ' | '\t'),
            Class::Cntrl => c.is_ascii_control(),
            Class::Graph => c.is_ascii_graphic(),
            Class::Print => c.is_ascii_graphic() || c == ' ',
        }
    }
}

#[derive(Debug)]
enum SetItem {
    Range(char, char),
    Class(Class, bool),
}

#[derive(Debug)]
enum Node {
    Char(char),
    Any,
    Set(Vec<SetItem>, bool),
    Bol,
    Eol,
    WordBoundary,
    WordStart,
    WordEnd,
    Cat(Vec<Node>),
    Alt(Vec<Node>),
    Rep(Box<Node>, u32, u32, bool),
}

fn regex_error(msg: &str) -> Error {
    Error::new(
        SqlState("2201B"),
        format!("invalid regular expression: {msg}"),
    )
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn eat(&mut self, c: char) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn alt(&mut self) -> Result<Node> {
        let mut branches = vec![self.cat()?];
        while self.eat('|') {
            branches.push(self.cat()?);
        }
        Ok(if branches.len() == 1 {
            branches.remove(0)
        } else {
            Node::Alt(branches)
        })
    }

    fn cat(&mut self) -> Result<Node> {
        let mut items = Vec::new();
        while let Some(c) = self.peek() {
            if c == '|' || c == ')' {
                break;
            }
            let atom = self.atom()?;
            items.push(self.quantified(atom)?);
        }
        Ok(Node::Cat(items))
    }

    fn number(&mut self) -> Option<u32> {
        let start = self.pos;
        while self.peek().is_some_and(|c| c.is_ascii_digit()) {
            self.pos += 1;
        }
        self.chars[start..self.pos]
            .iter()
            .collect::<String>()
            .parse()
            .ok()
    }

    fn quantified(&mut self, mut atom: Node) -> Result<Node> {
        loop {
            let (min, max) = match self.peek() {
                Some('*') => (0, u32::MAX),
                Some('+') => (1, u32::MAX),
                Some('?') => (0, 1),
                Some('{') => {
                    let save = self.pos;
                    self.pos += 1;
                    let Some(min) = self.number() else {
                        self.pos = save;
                        return Err(regex_error("invalid repetition count(s)"));
                    };
                    let max = if self.eat(',') {
                        if self.peek() == Some('}') {
                            u32::MAX
                        } else {
                            self.number()
                                .ok_or_else(|| regex_error("invalid repetition count(s)"))?
                        }
                    } else {
                        min
                    };
                    if self.peek() != Some('}') || min > max || max > 255 && max != u32::MAX {
                        return Err(regex_error("invalid repetition count(s)"));
                    }
                    (min, max)
                }
                _ => return Ok(atom),
            };
            self.pos += 1;
            let greedy = !self.eat('?');
            atom = Node::Rep(Box::new(atom), min, max, greedy);
        }
    }

    fn atom(&mut self) -> Result<Node> {
        let c = self.peek().ok_or_else(|| regex_error("unexpected end"))?;
        self.pos += 1;
        match c {
            '.' => Ok(Node::Any),
            '^' => Ok(Node::Bol),
            '$' => Ok(Node::Eol),
            '(' => {
                if self.eat('?') && !self.eat(':') {
                    return Err(regex_error("invalid embedded option"));
                }
                let inner = self.alt()?;
                if !self.eat(')') {
                    return Err(regex_error("parentheses () not balanced"));
                }
                Ok(inner)
            }
            '[' => self.bracket(),
            '*' | '+' | '?' => Err(regex_error("quantifier operand invalid")),
            '\\' => self.escape(),
            c => Ok(Node::Char(c)),
        }
    }

    fn escape(&mut self) -> Result<Node> {
        let c = self
            .peek()
            .ok_or_else(|| regex_error("invalid escape \\ sequence"))?;
        self.pos += 1;
        let set = |class, neg| Node::Set(vec![SetItem::Class(class, false)], neg);
        Ok(match c {
            'd' => set(Class::Digit, false),
            'D' => set(Class::Digit, true),
            'w' => set(Class::Word, false),
            'W' => set(Class::Word, true),
            's' => set(Class::Space, false),
            'S' => set(Class::Space, true),
            'y' | 'b' => Node::WordBoundary,
            'm' => Node::WordStart,
            'M' => Node::WordEnd,
            'A' => Node::Bol,
            'Z' => Node::Eol,
            'n' => Node::Char('\n'),
            't' => Node::Char('\t'),
            'r' => Node::Char('\r'),
            'f' => Node::Char('\x0c'),
            'v' => Node::Char('\x0b'),
            'a' => Node::Char('\x07'),
            'e' => Node::Char('\x1b'),
            c if c.is_ascii_alphanumeric() => {
                return Err(regex_error("invalid escape \\ sequence"));
            }
            c => Node::Char(c),
        })
    }

    fn bracket(&mut self) -> Result<Node> {
        let neg = self.eat('^');
        let mut items = Vec::new();
        let mut first = true;
        loop {
            let c = self
                .peek()
                .ok_or_else(|| regex_error("brackets [] not balanced"))?;
            self.pos += 1;
            if c == ']' && !first {
                break;
            }
            first = false;
            let lo = if c == '[' && self.peek() == Some(':') {
                let rest: String = self.chars[self.pos + 1..].iter().collect();
                let end = rest
                    .find(":]")
                    .ok_or_else(|| regex_error("brackets [] not balanced"))?;
                let name = &rest[..end];
                let class = match name {
                    "alpha" => Class::Alpha,
                    "digit" => Class::Digit,
                    "alnum" => Class::Alnum,
                    "upper" => Class::Upper,
                    "lower" => Class::Lower,
                    "space" => Class::Space,
                    "punct" => Class::Punct,
                    "xdigit" => Class::Xdigit,
                    "blank" => Class::Blank,
                    "cntrl" => Class::Cntrl,
                    "graph" => Class::Graph,
                    "print" => Class::Print,
                    _ => return Err(regex_error("invalid character class")),
                };
                self.pos += 1 + name.chars().count() + 2;
                items.push(SetItem::Class(class, false));
                continue;
            } else if c == '\\' {
                let e = self
                    .peek()
                    .ok_or_else(|| regex_error("brackets [] not balanced"))?;
                self.pos += 1;
                match e {
                    'd' | 'D' | 'w' | 'W' | 's' | 'S' => {
                        let class = match e.to_ascii_lowercase() {
                            'd' => Class::Digit,
                            'w' => Class::Word,
                            _ => Class::Space,
                        };
                        items.push(SetItem::Class(class, e.is_ascii_uppercase()));
                        continue;
                    }
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    e => e,
                }
            } else {
                c
            };
            if self.peek() == Some('-') && self.chars.get(self.pos + 1).is_some_and(|n| *n != ']') {
                self.pos += 1;
                let mut hi = self.peek().unwrap_or(']');
                self.pos += 1;
                if hi == '\\' {
                    hi = self.peek().unwrap_or('\\');
                    self.pos += 1;
                }
                if hi < lo {
                    return Err(regex_error("invalid character range"));
                }
                items.push(SetItem::Range(lo, hi));
            } else {
                items.push(SetItem::Range(lo, lo));
            }
        }
        Ok(Node::Set(items, neg))
    }
}

/// A compiled regular expression.
#[derive(Debug)]
pub struct Regex {
    root: Node,
    icase: bool,
}

fn is_word(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

impl Regex {
    pub fn new(pattern: &str, icase: bool) -> Result<Regex> {
        let mut p = Parser {
            chars: pattern.chars().collect(),
            pos: 0,
        };
        let root = p.alt()?;
        if p.pos < p.chars.len() {
            return Err(regex_error("parentheses () not balanced"));
        }
        Ok(Regex { root, icase })
    }

    /// Whether the pattern matches anywhere in `text`.
    pub fn is_match(&self, text: &str) -> bool {
        let t: Vec<char> = text.chars().collect();
        (0..=t.len()).any(|start| self.m(&self.root, &t, start, &mut |_| true))
    }

    fn eq(&self, a: char, b: char) -> bool {
        a == b || (self.icase && a.eq_ignore_ascii_case(&b))
    }

    fn set_matches(&self, items: &[SetItem], neg: bool, c: char) -> bool {
        let test = |c: char| {
            items.iter().any(|i| match i {
                SetItem::Range(lo, hi) => *lo <= c && c <= *hi,
                SetItem::Class(cl, n) => cl.matches(c) != *n,
            })
        };
        let mut hit = test(c);
        if !hit && self.icase {
            hit = test(c.to_ascii_lowercase()) || test(c.to_ascii_uppercase());
        }
        hit != neg
    }

    #[allow(clippy::many_single_char_names)]
    fn m(&self, n: &Node, t: &[char], i: usize, k: &mut dyn FnMut(usize) -> bool) -> bool {
        match n {
            Node::Char(c) => t.get(i).is_some_and(|x| self.eq(*x, *c)) && k(i + 1),
            Node::Any => i < t.len() && k(i + 1),
            Node::Set(items, neg) => {
                t.get(i).is_some_and(|c| self.set_matches(items, *neg, *c)) && k(i + 1)
            }
            Node::Bol => i == 0 && k(i),
            Node::Eol => i == t.len() && k(i),
            Node::WordBoundary | Node::WordStart | Node::WordEnd => {
                let before = i > 0 && is_word(t[i - 1]);
                let after = i < t.len() && is_word(t[i]);
                let ok = match n {
                    Node::WordBoundary => before != after,
                    Node::WordStart => !before && after,
                    _ => before && !after,
                };
                ok && k(i)
            }
            Node::Cat(items) => self.cat(items, t, i, k),
            Node::Alt(branches) => branches.iter().any(|b| self.m(b, t, i, k)),
            Node::Rep(inner, min, max, greedy) => self.rep(inner, *min, *max, *greedy, 0, t, i, k),
        }
    }

    fn cat(&self, items: &[Node], t: &[char], i: usize, k: &mut dyn FnMut(usize) -> bool) -> bool {
        match items.split_first() {
            None => k(i),
            Some((first, rest)) => self.m(first, t, i, &mut |j| self.cat(rest, t, j, k)),
        }
    }

    #[allow(clippy::too_many_arguments, clippy::if_same_then_else)]
    fn rep(
        &self,
        n: &Node,
        min: u32,
        max: u32,
        greedy: bool,
        count: u32,
        t: &[char],
        i: usize,
        k: &mut dyn FnMut(usize) -> bool,
    ) -> bool {
        let more = |k: &mut dyn FnMut(usize) -> bool| {
            count < max
                && self.m(n, t, i, &mut |j| {
                    if j == i && count >= min {
                        return false;
                    }
                    self.rep(n, min, max, greedy, count + 1, t, j, k)
                })
        };
        if count < min {
            more(k)
        } else if greedy {
            more(k) || k(i)
        } else {
            k(i) || more(k)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Regex;

    fn is(p: &str, s: &str) -> bool {
        Regex::new(p, false).unwrap().is_match(s)
    }

    #[test]
    fn basics() {
        assert!(is("a.c", "xabcx"));
        assert!(!is("^a.c$", "xabcx"));
        assert!(is("^(ab|cd)+$", "abcdab"));
        assert!(is("^[a-c]{2,3}$", "abc"));
        assert!(!is("^[a-c]{2,3}$", "abcd"));
        assert!(is("\\d+", "ab12"));
        assert!(is("[[:upper:]]", "aBc"));
        assert!(is("^$", ""));
        assert!(Regex::new("(", false).is_err());
        assert!(Regex::new("ABC", true).unwrap().is_match("xabcx"));
    }
}
