//! `PostgreSQL` isolationtester の spec ファイルのパーサ。
//!
//! 文法は `PostgreSQL` 17 の `src/test/isolation/specparse.y` と `specscanner.l` に合わせる。
//!
//! ```text
//! TestSpec   := setup* teardown? session+ permutation*
//! session    := "session" ident setup? step+ teardown?
//! step       := "step" ident sqlblock
//! permutation:= "permutation" pstep+
//! pstep      := ident | ident "(" blocker ("," blocker)* ")"
//! blocker    := ident | ident "notices" INTEGER | "*"
//! ```
//!
//! SQL ブロック `{ ... }` は、開き括弧直後と閉じ括弧直前の空白（スペース・タブ・CR・FF）
//! だけを取り除く。改行は取り除かない（isolationtester と同じで、出力にもそのまま現れる）。

use std::fmt;

/// spec ファイル全体。
#[derive(Debug, Clone)]
pub(crate) struct TestSpec {
    pub(crate) setup_sqls: Vec<String>,
    pub(crate) teardown_sql: Option<String>,
    pub(crate) sessions: Vec<Session>,
    pub(crate) permutations: Vec<Permutation>,
}

/// `session <name> { ... }` に相当する接続の定義。
#[derive(Debug, Clone)]
pub(crate) struct Session {
    pub(crate) name: String,
    pub(crate) setup_sql: Option<String>,
    pub(crate) steps: Vec<Step>,
    pub(crate) teardown_sql: Option<String>,
}

/// `step <name> { SQL }`。
#[derive(Debug, Clone)]
pub(crate) struct Step {
    pub(crate) name: String,
    pub(crate) sql: String,
}

/// `permutation ...` の 1 行。
#[derive(Debug, Clone)]
pub(crate) struct Permutation {
    pub(crate) steps: Vec<PermutationStep>,
}

/// permutation 内の 1 ステップ（完了報告を遅らせるマーカー付き）。
#[derive(Debug, Clone)]
pub(crate) struct PermutationStep {
    pub(crate) name: String,
    pub(crate) blockers: Vec<BlockerSpec>,
}

/// 完了報告を遅らせる条件（構文上の形）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BlockerSpec {
    /// `(*)`: 起動直後に必ず `<waiting ...>` と報告する。
    Once,
    /// `(other)`: 他のステップが完了するまで完了を報告しない。
    OtherStep(String),
    /// `(other notices N)`: 他のステップのセッションが N 個の NOTICE を返すまで待つ。
    Notices(String, usize),
}

/// パースエラー。`message` は isolationtester が標準エラーに出す文言そのまま
/// （`syntax error at line N` など。`pg_isolation_regress` では期待ファイルに現れる）。
#[derive(Debug)]
pub(crate) struct ParseError {
    pub(crate) message: String,
}

impl ParseError {
    /// `spec_yyerror` と同じ `"<msg> at line <N>"`。
    fn at(line: usize, message: &str) -> Self {
        Self {
            message: format!("{message} at line {line}"),
        }
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ParseError {}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Tok {
    Notices,
    Permutation,
    Session,
    Setup,
    Step,
    Teardown,
    Ident(String),
    Sql(String),
    Int(usize),
    LParen,
    RParen,
    Comma,
    Star,
}

fn is_space(c: char) -> bool {
    matches!(c, ' ' | '\t' | '\r' | '\u{c}')
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_' || !c.is_ascii()
}

fn is_ident_cont(c: char) -> bool {
    is_ident_start(c) || c.is_ascii_digit() || c == '$'
}

#[allow(clippy::too_many_lines)]
fn lex(src: &str) -> Result<Vec<(Tok, usize)>, ParseError> {
    let chars: Vec<char> = src.chars().collect();
    let mut toks = Vec::new();
    let mut i = 0;
    let mut line = 1;
    let err = |line: usize, message: String| ParseError::at(line, &message);
    while i < chars.len() {
        let c = chars[i];
        if c == '\n' {
            line += 1;
            i += 1;
        } else if is_space(c) {
            i += 1;
        } else if c == '#' {
            while i < chars.len() && chars[i] != '\n' && chars[i] != '\r' {
                i += 1;
            }
        } else if is_ident_start(c) {
            let start = i;
            while i < chars.len() && is_ident_cont(chars[i]) {
                i += 1;
            }
            let word: String = chars[start..i].iter().collect();
            let tok = match word.as_str() {
                "notices" => Tok::Notices,
                "permutation" => Tok::Permutation,
                "session" => Tok::Session,
                "setup" => Tok::Setup,
                "step" => Tok::Step,
                "teardown" => Tok::Teardown,
                _ => Tok::Ident(word),
            };
            toks.push((tok, line));
        } else if c == '"' {
            let start_line = line;
            i += 1;
            let mut s = String::new();
            loop {
                match chars.get(i) {
                    None => return Err(err(line, "unterminated quoted identifier".into())),
                    Some('\n') => {
                        return Err(err(line, "unexpected newline in quoted identifier".into()));
                    }
                    Some('"') if chars.get(i + 1) == Some(&'"') => {
                        s.push('"');
                        i += 2;
                    }
                    Some('"') => {
                        i += 1;
                        break;
                    }
                    Some(&ch) => {
                        s.push(ch);
                        i += 1;
                    }
                }
            }
            toks.push((Tok::Ident(s), start_line));
        } else if c == '{' {
            i += 1;
            while i < chars.len() && is_space(chars[i]) {
                i += 1;
            }
            let start = i;
            while i < chars.len() && chars[i] != '}' {
                if chars[i] == '\n' {
                    line += 1;
                }
                i += 1;
            }
            if i >= chars.len() {
                return Err(err(line, "unterminated sql block".into()));
            }
            let mut end = i;
            while end > start && is_space(chars[end - 1]) {
                end -= 1;
            }
            let sql: String = chars[start..end].iter().collect();
            i += 1; // '}'
            // bison がエラーを報告する時点の yyline は、先読みしたトークンの末尾の行。
            toks.push((Tok::Sql(sql), line));
        } else if c.is_ascii_digit() {
            let start = i;
            while i < chars.len() && chars[i].is_ascii_digit() {
                i += 1;
            }
            let digits: String = chars[start..i].iter().collect();
            let n = digits
                .parse()
                .map_err(|_| err(line, format!("invalid integer \"{digits}\"")))?;
            toks.push((Tok::Int(n), line));
        } else {
            let tok = match c {
                '(' => Tok::LParen,
                ')' => Tok::RParen,
                ',' => Tok::Comma,
                '*' => Tok::Star,
                _ => {
                    // specscanner.l はこれだけ spec_yyerror を通さず独自の書式で出す。
                    return Err(ParseError {
                        message: format!(
                            "syntax error at line {line}: unexpected character \"{c}\""
                        ),
                    });
                }
            };
            toks.push((tok, line));
            i += 1;
        }
    }
    Ok(toks)
}

struct Parser {
    toks: Vec<(Tok, usize)>,
    pos: usize,
    last_line: usize,
}

impl Parser {
    fn peek(&self) -> Option<&Tok> {
        self.toks.get(self.pos).map(|(t, _)| t)
    }

    fn line(&self) -> usize {
        self.toks.get(self.pos).map_or(self.last_line, |(_, l)| *l)
    }

    fn next(&mut self) -> Option<Tok> {
        let t = self.toks.get(self.pos).map(|(t, _)| t.clone());
        if t.is_some() {
            self.pos += 1;
        }
        t
    }

    /// bison の構文エラー。isolationtester は詳細を出さず `syntax error at line N` だけを出す
    /// ので、それに合わせる（`expected` は使わない）。
    fn error(&self, _expected: &str) -> ParseError {
        ParseError::at(self.line(), "syntax error")
    }

    fn eat(&mut self, tok: &Tok) -> bool {
        if self.peek() == Some(tok) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn ident(&mut self) -> Result<String, ParseError> {
        if let Some(Tok::Ident(s)) = self.peek() {
            let s = s.clone();
            self.pos += 1;
            Ok(s)
        } else {
            Err(self.error("identifier"))
        }
    }

    fn sql(&mut self) -> Result<String, ParseError> {
        if let Some(Tok::Sql(s)) = self.peek() {
            let s = s.clone();
            self.pos += 1;
            Ok(s)
        } else {
            Err(self.error("sql block"))
        }
    }

    fn opt_block(&mut self, kw: &Tok) -> Result<Option<String>, ParseError> {
        if self.eat(kw) {
            Ok(Some(self.sql()?))
        } else {
            Ok(None)
        }
    }

    fn spec(&mut self) -> Result<TestSpec, ParseError> {
        let mut setup_sqls = Vec::new();
        while self.eat(&Tok::Setup) {
            setup_sqls.push(self.sql()?);
        }
        let teardown_sql = self.opt_block(&Tok::Teardown)?;
        let mut sessions = Vec::new();
        while self.eat(&Tok::Session) {
            let name = self.ident()?;
            let setup_sql = self.opt_block(&Tok::Setup)?;
            let mut steps = Vec::new();
            while self.eat(&Tok::Step) {
                let name = self.ident()?;
                let sql = self.sql()?;
                steps.push(Step { name, sql });
            }
            if steps.is_empty() {
                return Err(self.error("\"step\""));
            }
            let teardown_sql = self.opt_block(&Tok::Teardown)?;
            sessions.push(Session {
                name,
                setup_sql,
                steps,
                teardown_sql,
            });
        }
        if sessions.is_empty() {
            return Err(self.error("\"session\""));
        }
        let mut permutations = Vec::new();
        while self.eat(&Tok::Permutation) {
            let mut steps = Vec::new();
            while let Some(Tok::Ident(_)) = self.peek() {
                let name = self.ident()?;
                let mut blockers = Vec::new();
                if self.eat(&Tok::LParen) {
                    loop {
                        if self.eat(&Tok::Star) {
                            blockers.push(BlockerSpec::Once);
                        } else {
                            let other = self.ident()?;
                            if self.eat(&Tok::Notices) {
                                if let Some(Tok::Int(n)) = self.next() {
                                    blockers.push(BlockerSpec::Notices(other, n));
                                } else {
                                    self.pos -= 1;
                                    return Err(self.error("integer"));
                                }
                            } else {
                                blockers.push(BlockerSpec::OtherStep(other));
                            }
                        }
                        if self.eat(&Tok::Comma) {
                            continue;
                        }
                        if self.eat(&Tok::RParen) {
                            break;
                        }
                        return Err(self.error("\",\" or \")\""));
                    }
                }
                steps.push(PermutationStep { name, blockers });
            }
            if steps.is_empty() {
                return Err(self.error("step name"));
            }
            permutations.push(Permutation { steps });
        }
        if self.peek().is_some() {
            return Err(self.error("end of file"));
        }
        Ok(TestSpec {
            setup_sqls,
            teardown_sql,
            sessions,
            permutations,
        })
    }
}

/// spec ファイルの内容をパースする。
pub(crate) fn parse(src: &str) -> Result<TestSpec, ParseError> {
    let toks = lex(src)?;
    // 入力の終わりで失敗したときの行番号は、全改行を数え終えた後の yyline。
    let last_line = 1 + src.bytes().filter(|&b| b == b'\n').count();
    Parser {
        toks,
        pos: 0,
        last_line,
    }
    .spec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_basic_spec() {
        let src = r#"
# comment
setup { CREATE TABLE t (a int); }
setup
{
  INSERT INTO t VALUES (1);
}
teardown { DROP TABLE t; }

session s1
setup { BEGIN; }
step s1a { SELECT * FROM t; }
step "s1 b"	{ COMMIT; }

session "s2"
step s2a {
	UPDATE t SET a = 2;
}
teardown { ROLLBACK; }

permutation s1a s2a(*) "s1 b"
permutation s2a(s1a, s1a notices 2) s1a
"#;
        let spec = parse(src).unwrap();
        assert_eq!(spec.setup_sqls.len(), 2);
        assert_eq!(spec.setup_sqls[0], "CREATE TABLE t (a int);");
        assert_eq!(spec.setup_sqls[1], "\n  INSERT INTO t VALUES (1);\n");
        assert_eq!(spec.teardown_sql.as_deref(), Some("DROP TABLE t;"));
        assert_eq!(spec.sessions.len(), 2);
        assert_eq!(spec.sessions[0].steps[1].name, "s1 b");
        assert_eq!(spec.sessions[1].name, "s2");
        assert_eq!(spec.sessions[1].steps[0].sql, "\n\tUPDATE t SET a = 2;\n");
        assert_eq!(spec.sessions[1].teardown_sql.as_deref(), Some("ROLLBACK;"));
        assert_eq!(spec.permutations.len(), 2);
        assert_eq!(
            spec.permutations[0].steps[1].blockers,
            vec![BlockerSpec::Once]
        );
        assert_eq!(
            spec.permutations[1].steps[0].blockers,
            vec![
                BlockerSpec::OtherStep("s1a".into()),
                BlockerSpec::Notices("s1a".into(), 2)
            ]
        );
    }

    #[test]
    fn quoted_keyword_is_identifier() {
        let spec = parse("session \"step\" step \"permutation\" { SELECT 1; }").unwrap();
        assert_eq!(spec.sessions[0].name, "step");
        assert_eq!(spec.sessions[0].steps[0].name, "permutation");
        assert!(spec.permutations.is_empty());
    }

    #[test]
    fn rejects_missing_session() {
        assert!(parse("setup { SELECT 1; }").is_err());
        assert!(parse("session s1 step a { SELECT 1; ").is_err());
        assert!(parse("session s1 step a { SELECT 1; } permutation").is_err());
    }

    /// 文言と行番号は `PostgreSQL` 17 の isolationtester の出力に合わせてある。
    #[test]
    fn error_messages_match_isolationtester() {
        let msg = |src: &str| parse(src).unwrap_err().to_string();
        assert_eq!(
            msg("session s1\nstep s1a { SELECT 1; }\npermutation s1a -\n"),
            "syntax error at line 3: unexpected character \"-\""
        );
        assert_eq!(
            msg("session s1\nstep s1a { SELECT 1; }\npermutation s1a(\n"),
            "syntax error at line 4"
        );
        assert_eq!(
            msg(
                "session s1\nstep s1a { SELECT 1;\n}\nstep s1b {\n SELECT 2;\n} permutation s1a s1b(s1a notices x)\n\n"
            ),
            "syntax error at line 6"
        );
        assert_eq!(msg("# only comments\n\n\n"), "syntax error at line 4");
        assert_eq!(
            msg("session s1\nstep s1a { SELECT 1;\n"),
            "unterminated sql block at line 3"
        );
        assert_eq!(
            msg("session \"s1\n"),
            "unexpected newline in quoted identifier at line 1"
        );
    }
}
