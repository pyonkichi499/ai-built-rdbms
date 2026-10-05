//! sqllogictest ファイルの最小限のパーサ（lint 用）。

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    /// `statement ok | error | count N`。`error` のとき true
    Statement {
        error: bool,
        count: Option<u64>,
    },
    /// `query <型文字> [sort]`。`error` のとき true
    Query {
        types: String,
        sort: String,
        error: bool,
    },
    Other,
}

#[derive(Debug, Clone)]
pub struct Record {
    /// ディレクティブの行（1 始まり）
    pub line: usize,
    pub kind: Kind,
    /// ディレクティブの行そのもの
    pub directive: String,
    pub sql: String,
    /// SQL の最初の行（1 始まり）
    pub sql_line: usize,
    pub expected: Vec<String>,
    /// `onlyif X` / `skipif X`（true = onlyif）と、その行（1 始まり）
    pub conds: Vec<(bool, String, usize)>,
}

pub struct SltFile {
    pub lines: Vec<String>,
    pub records: Vec<Record>,
}

pub fn parse(text: &str) -> SltFile {
    let lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut records = Vec::new();
    let mut i = 0;
    let mut conds: Vec<(bool, String, usize)> = Vec::new();
    while i < lines.len() {
        let l = lines[i].trim();
        if l.is_empty() {
            conds.clear();
            i += 1;
            continue;
        }
        if l.starts_with('#') {
            i += 1;
            continue;
        }
        let mut words = l.split_whitespace();
        let w0 = words.next().unwrap_or("");
        match w0 {
            "onlyif" | "skipif" => {
                conds.push((
                    w0 == "onlyif",
                    words.next().unwrap_or("").to_string(),
                    i + 1,
                ));
                i += 1;
                continue;
            }
            "connection" => {
                let _ = words.next();
                i += 1;
                continue;
            }
            _ => {}
        }
        let rest: Vec<&str> = words.collect();
        let kind = match w0 {
            "statement" => {
                let error = rest.first() == Some(&"error");
                let count = if rest.first() == Some(&"count") {
                    rest.get(1).and_then(|n| n.parse().ok())
                } else {
                    None
                };
                Kind::Statement { error, count }
            }
            "query" => {
                let error = rest.first() == Some(&"error");
                let types = if error {
                    String::new()
                } else {
                    rest.first().unwrap_or(&"").to_string()
                };
                let sort = if error {
                    String::new()
                } else {
                    rest.get(1).unwrap_or(&"").to_string()
                };
                Kind::Query { types, sort, error }
            }
            _ => Kind::Other,
        };
        let line = i + 1;
        let directive = l.to_string();
        i += 1;
        let sql_line = i + 1;
        let mut sql = String::new();
        while i < lines.len() && !lines[i].trim().is_empty() && lines[i].trim() != "----" {
            sql.push_str(&lines[i]);
            sql.push('\n');
            i += 1;
        }
        let mut expected = Vec::new();
        if i < lines.len() && lines[i].trim() == "----" {
            i += 1;
            while i < lines.len() && !lines[i].trim().is_empty() {
                expected.push(lines[i].trim().to_string());
                i += 1;
            }
        }
        records.push(Record {
            line,
            kind,
            directive,
            sql,
            sql_line,
            expected,
            conds: std::mem::take(&mut conds),
        });
    }
    SltFile { lines, records }
}

/// SQL をトークンに分ける（小文字化。文字列リテラルは `'` 1 トークン、`--` コメントは捨てる）。
pub fn tokens(sql: &str) -> Vec<String> {
    let cs: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < cs.len() {
        let c = cs[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && cs.get(i + 1) == Some(&'-') {
            while i < cs.len() && cs[i] != '\n' {
                i += 1;
            }
        } else if c == '\'' {
            i += 1;
            while i < cs.len() {
                if cs[i] == '\'' {
                    if cs.get(i + 1) == Some(&'\'') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
            out.push("'".to_string());
        } else if c == '"' {
            let mut s = String::new();
            i += 1;
            while i < cs.len() && cs[i] != '"' {
                s.push(cs[i]);
                i += 1;
            }
            i += 1;
            out.push(s);
        } else if c.is_alphanumeric() || c == '_' {
            let mut s = String::new();
            while i < cs.len() && (cs[i].is_alphanumeric() || cs[i] == '_' || cs[i] == '$') {
                s.push(cs[i].to_ascii_lowercase());
                i += 1;
            }
            out.push(s);
        } else {
            out.push(c.to_string());
            i += 1;
        }
    }
    out
}

/// トークン列を `;` で文に分ける。
pub fn split_statements(toks: &[String]) -> Vec<Vec<String>> {
    toks.split(|t| t == ";")
        .filter(|s| !s.is_empty())
        .map(<[String]>::to_vec)
        .collect()
}
