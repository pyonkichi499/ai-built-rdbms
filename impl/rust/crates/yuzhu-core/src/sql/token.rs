//! Tokens produced by the lexer, and PostgreSQL's keyword categories.

use crate::error::Span;

/// The kind of a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    /// An identifier or keyword. Unquoted words are already case-folded
    /// (ASCII only, like PostgreSQL); `quoted` is true for `"..."`.
    /// Keywords are recognized by the parser from the folded text of an
    /// unquoted word (see [`keyword_category`]).
    Word {
        value: String,
        quoted: bool,
    },
    /// Integer literal, normalized to plain decimal digits (underscores
    /// removed, `0x`/`0o`/`0b` converted).
    Integer(String),
    /// Decimal / exponent literal text (underscores removed).
    Decimal(String),
    /// String constant after escape processing and concatenation of
    /// newline-separated adjacent literals.
    String(String),
    /// `$n`.
    Param(u32),
    /// An operator. Single-character operators (`+ - * / % ^ < > =`) and
    /// multi-character ones alike; `!=` is normalized to `<>`.
    Op(String),
    /// `::`
    Typecast,
    /// `:=`
    ColonEquals,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Comma,
    Semicolon,
    Colon,
    Dot,
    /// Any other character (for example a lone `$` or `\`); always a
    /// syntax error in the parser.
    Other(char),
    /// Placeholder for the position where lexing failed; the parser reports
    /// the stored lexer error when it reaches this token.
    LexError,
    /// End of input.
    Eof,
}

/// A token with its byte span in the query text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

impl Token {
    /// The keyword text if this is an unquoted word.
    pub fn keyword(&self) -> Option<&str> {
        match &self.kind {
            TokenKind::Word {
                value,
                quoted: false,
            } => Some(value.as_str()),
            _ => None,
        }
    }

    /// True if this is the unquoted word `kw` (lower case).
    pub fn is_kw(&self, kw: &str) -> bool {
        self.keyword() == Some(kw)
    }

    /// True if this is the operator `op`.
    pub fn is_op(&self, op: &str) -> bool {
        matches!(&self.kind, TokenKind::Op(o) if o == op)
    }
}

/// PostgreSQL keyword categories (`kwlist.h`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeywordCategory {
    /// Not a keyword, or an unreserved keyword: usable anywhere a name is.
    Unreserved,
    /// Usable as a column / table name, but not as a function or type name.
    ColName,
    /// Usable as a function or type name, but not as a column name.
    TypeFuncName,
    /// Fully reserved (usable only as a column label after `AS`, or after `.`).
    Reserved,
}

const RESERVED: &[&str] = &[
    "all",
    "analyse",
    "analyze",
    "and",
    "any",
    "array",
    "as",
    "asc",
    "asymmetric",
    "both",
    "case",
    "cast",
    "check",
    "collate",
    "column",
    "constraint",
    "create",
    "current_catalog",
    "current_date",
    "current_role",
    "current_time",
    "current_timestamp",
    "current_user",
    "default",
    "deferrable",
    "desc",
    "distinct",
    "do",
    "else",
    "end",
    "except",
    "false",
    "fetch",
    "for",
    "foreign",
    "from",
    "grant",
    "group",
    "having",
    "in",
    "initially",
    "intersect",
    "into",
    "lateral",
    "leading",
    "limit",
    "localtime",
    "localtimestamp",
    "not",
    "null",
    "offset",
    "on",
    "only",
    "or",
    "order",
    "placing",
    "primary",
    "references",
    "returning",
    "select",
    "session_user",
    "some",
    "symmetric",
    "system_user",
    "table",
    "then",
    "to",
    "trailing",
    "true",
    "union",
    "unique",
    "user",
    "using",
    "variadic",
    "when",
    "where",
    "window",
    "with",
];

const TYPE_FUNC_NAME: &[&str] = &[
    "authorization",
    "binary",
    "collation",
    "concurrently",
    "cross",
    "current_schema",
    "freeze",
    "full",
    "ilike",
    "inner",
    "is",
    "isnull",
    "join",
    "left",
    "like",
    "natural",
    "notnull",
    "outer",
    "overlaps",
    "right",
    "similar",
    "tablesample",
    "verbose",
];

const COL_NAME: &[&str] = &[
    "between",
    "bigint",
    "bit",
    "boolean",
    "char",
    "character",
    "coalesce",
    "dec",
    "decimal",
    "exists",
    "extract",
    "float",
    "greatest",
    "grouping",
    "inout",
    "int",
    "integer",
    "interval",
    "json",
    "json_array",
    "json_arrayagg",
    "json_exists",
    "json_object",
    "json_objectagg",
    "json_query",
    "json_scalar",
    "json_serialize",
    "json_table",
    "json_value",
    "least",
    "merge_action",
    "national",
    "nchar",
    "none",
    "normalize",
    "nullif",
    "numeric",
    "out",
    "overlay",
    "position",
    "precision",
    "real",
    "row",
    "setof",
    "smallint",
    "substring",
    "time",
    "timestamp",
    "treat",
    "trim",
    "values",
    "varchar",
    "xmlattributes",
    "xmlconcat",
    "xmlelement",
    "xmlexists",
    "xmlforest",
    "xmlnamespaces",
    "xmlparse",
    "xmlpi",
    "xmlroot",
    "xmlserialize",
    "xmltable",
];

/// Keywords that can be a column label only with `AS` (`AS_LABEL` in
/// `kwlist.h`); every other keyword may be a bare label (`SELECT 1 name`).
const AS_LABEL: &[&str] = &[
    "array",
    "as",
    "char",
    "character",
    "create",
    "day",
    "except",
    "fetch",
    "filter",
    "for",
    "from",
    "grant",
    "group",
    "having",
    "hour",
    "intersect",
    "into",
    "isnull",
    "limit",
    "minute",
    "month",
    "notnull",
    "offset",
    "on",
    "order",
    "over",
    "precision",
    "returning",
    "second",
    "to",
    "union",
    "varying",
    "where",
    "window",
    "with",
    "within",
    "without",
    "year",
];

/// The category of an (unquoted, lower-case) word.
pub fn keyword_category(word: &str) -> KeywordCategory {
    if RESERVED.binary_search(&word).is_ok() {
        KeywordCategory::Reserved
    } else if TYPE_FUNC_NAME.binary_search(&word).is_ok() {
        KeywordCategory::TypeFuncName
    } else if COL_NAME.binary_search(&word).is_ok() {
        KeywordCategory::ColName
    } else {
        KeywordCategory::Unreserved
    }
}

/// True if the (unquoted, lower-case) word requires `AS` to be a column label.
pub fn requires_as_label(word: &str) -> bool {
    AS_LABEL.binary_search(&word).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_tables_are_sorted() {
        for table in [RESERVED, TYPE_FUNC_NAME, COL_NAME, AS_LABEL] {
            let mut sorted = table.to_vec();
            sorted.sort_unstable();
            assert_eq!(sorted, table);
        }
    }

    #[test]
    fn categories() {
        assert_eq!(keyword_category("select"), KeywordCategory::Reserved);
        assert_eq!(keyword_category("left"), KeywordCategory::TypeFuncName);
        assert_eq!(keyword_category("int"), KeywordCategory::ColName);
        assert_eq!(keyword_category("name"), KeywordCategory::Unreserved);
        assert_eq!(keyword_category("foo"), KeywordCategory::Unreserved);
        assert!(requires_as_label("from"));
        assert!(!requires_as_label("name"));
    }
}
