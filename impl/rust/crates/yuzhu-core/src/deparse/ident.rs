//! `quote_identifier`（`m4/10-explain-copy-compat.md` §4.7）。
//!
//! PostgreSQL の `quote_identifier` と同じ規則で、`Unreserved` 以外のキーワードも引用符で囲む。
//! `settings::quote_identifier`（M1。キーワードを見ない）は置き換えない。

use crate::sql::token::{KeywordCategory, keyword_category};

/// 識別子を SQL に埋め込める形にする。
///
/// 空文字列、先頭が `a-z` / `_` でない、2 文字目以降に `a-z` `0-9` `_` 以外がある、
/// `Unreserved` 以外のキーワード、のどれかなら `"..."`（中の `"` は `""`）で囲む。
pub fn quote_identifier(name: &str) -> String {
    if needs_quote(name) {
        format!("\"{}\"", name.replace('"', "\"\""))
    } else {
        name.to_owned()
    }
}

fn needs_quote(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return true;
    };
    if !(first.is_ascii_lowercase() || first == b'_') {
        return true;
    }
    if !bytes.all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_') {
        return true;
    }
    keyword_category(name) != KeywordCategory::Unreserved
}

/// `schema.name` の形（両方に [`quote_identifier`] を通す）。
pub fn quote_qualified(schema: &str, name: &str) -> String {
    format!("{}.{}", quote_identifier(schema), quote_identifier(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_names_are_not_quoted() {
        for n in ["t", "my_table", "_x", "a1", "abc_123"] {
            assert_eq!(quote_identifier(n), n);
        }
    }

    #[test]
    fn unusual_names_are_quoted() {
        assert_eq!(quote_identifier(""), "\"\"");
        assert_eq!(quote_identifier("MyTable"), "\"MyTable\"");
        assert_eq!(quote_identifier("1a"), "\"1a\"");
        assert_eq!(quote_identifier("a b"), "\"a b\"");
        assert_eq!(quote_identifier("a-b"), "\"a-b\"");
        assert_eq!(quote_identifier("a\"b"), "\"a\"\"b\"");
        assert_eq!(quote_identifier("é"), "\"é\"");
        assert_eq!(quote_identifier("*VALUES*"), "\"*VALUES*\"");
    }

    #[test]
    fn keywords_are_quoted_except_unreserved() {
        // Reserved / TypeFuncName / ColName.
        assert_eq!(quote_identifier("select"), "\"select\"");
        assert_eq!(quote_identifier("left"), "\"left\"");
        assert_eq!(quote_identifier("current_schema"), "\"current_schema\"");
        assert_eq!(quote_identifier("char"), "\"char\"");
        assert_eq!(quote_identifier("user"), "\"user\"");
        // Unreserved keywords stay bare.
        assert_eq!(quote_identifier("name"), "name");
        assert_eq!(quote_identifier("count"), "count");
    }

    #[test]
    fn qualified() {
        assert_eq!(quote_qualified("public", "t"), "public.t");
        assert_eq!(quote_qualified("public", "Order"), "public.\"Order\"");
    }
}
