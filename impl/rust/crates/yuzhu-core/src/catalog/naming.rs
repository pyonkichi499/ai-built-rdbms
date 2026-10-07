//! 自動命名（`m4/07-catalog-ddl.md` §5.10）。PostgreSQL の `makeObjectName` / `ChooseRelationName` /
//! `ChooseConstraintName` / `ChooseIndexName` / `ChooseIndexNameAddition` / `ChooseIndexColumnNames` の移植。
//!
//! analyzer と ddl が共有する（依存の向き: analyzer は ddl を `use` できない）。衝突の判定は
//! [`NameLookup`] を介して行い、「同じ文の中で先に決めたがカタログにまだ見えない名前」は `taken` で渡す。

#![allow(clippy::implicit_hasher)]

use std::collections::HashSet;

use crate::error::Result;
use crate::types::{MAX_IDENTIFIER_LENGTH, Oid};

/// `name1_name2_label`。63 バイト（`MAX_IDENTIFIER_LENGTH`）を超えるときは、長い方の名前を 1 文字ずつ
/// 短くする（PostgreSQL の `makeObjectName`）。UTF-8 の途中では切らない。
pub fn make_object_name(name1: &str, name2: Option<&str>, label: &str) -> String {
    let overhead = label.len() + 1 + usize::from(name2.is_some());
    let avail = MAX_IDENTIFIER_LENGTH.saturating_sub(overhead);
    let mut n1 = name1.len();
    let mut n2 = name2.map_or(0, str::len);
    while n1 + n2 > avail {
        if n1 > n2 {
            n1 -= 1;
        } else {
            n2 -= 1;
        }
    }
    let mut out = clip(name1, n1).to_owned();
    if let Some(n) = name2 {
        out.push('_');
        out.push_str(clip(n, n2));
    }
    out.push('_');
    out.push_str(label);
    out
}

/// `s` の先頭から高々 `n` バイト（文字の境界まで）。
fn clip(s: &str, mut n: usize) -> &str {
    if n >= s.len() {
        return s;
    }
    while !s.is_char_boundary(n) {
        n -= 1;
    }
    &s[..n]
}

/// 索引の列名の並び（`attname`）。重複は `origname + "1"`、`"2"` …（`ChooseIndexColumnNames`）。
/// 元の名前は、数字を付けても 63 バイトに収まるように切り詰める。
pub fn choose_index_column_names(column_names: &[&str]) -> Vec<String> {
    let mut result: Vec<String> = Vec::with_capacity(column_names.len());
    for orig in column_names {
        let mut name = (*orig).to_owned();
        let mut i = 0u32;
        while result.contains(&name) {
            i += 1;
            let digits = i.to_string();
            let room = MAX_IDENTIFIER_LENGTH.saturating_sub(digits.len());
            name = format!("{}{digits}", clip(orig, room));
        }
        result.push(name);
    }
    result
}

/// 列名を `_` でつないだ文字列。長さが 63 に達したら打ち切る（`ChooseIndexNameAddition`）。
pub fn choose_index_name_addition(index_column_names: &[String]) -> String {
    let mut buf = String::new();
    for name in index_column_names {
        if !buf.is_empty() {
            buf.push('_');
        }
        let room = MAX_IDENTIFIER_LENGTH.saturating_sub(buf.len());
        buf.push_str(clip(name, room));
        if buf.len() >= MAX_IDENTIFIER_LENGTH {
            break;
        }
    }
    buf
}

/// 名前の衝突の判定。実装は ddl の中の小さな構造体（`StatementCatalog` と `CatalogStore` を使う）で、
/// 解析側も同じ trait を満たす。
pub trait NameLookup {
    /// 表・索引・シーケンスのどれかに、この名前があるか。
    fn relation_exists(&self, nsp: Oid, name: &str) -> Result<bool>;
    /// `pg_constraint` に、この名前があるか（名前空間内のどの表の制約でも）。
    fn constraint_exists(&self, nsp: Oid, name: &str) -> Result<bool>;
}

/// `ChooseRelationName`。衝突したら `label` の後ろに `1`、`2` … を付けて探し直す。
/// `is_constraint` が真なら、同じ名前の制約とも衝突させない（制約が索引と同じ名前を持つため）。
pub fn choose_relation_name(
    name1: &str,
    name2: Option<&str>,
    label: &str,
    nsp: Oid,
    is_constraint: bool,
    lookup: &dyn NameLookup,
    taken: &HashSet<String>,
) -> Result<String> {
    let mut pass = 0u32;
    let mut modlabel = label.to_owned();
    loop {
        let relname = make_object_name(name1, name2, &modlabel);
        let clash = taken.contains(&relname)
            || lookup.relation_exists(nsp, &relname)?
            || (is_constraint && lookup.constraint_exists(nsp, &relname)?);
        if !clash {
            return Ok(relname);
        }
        pass += 1;
        modlabel = format!("{label}{pass}");
    }
}

/// `ChooseConstraintName`。衝突の判定は制約の名前だけ（リレーションの名前は見ない）。
pub fn choose_constraint_name(
    name1: &str,
    name2: Option<&str>,
    label: &str,
    nsp: Oid,
    lookup: &dyn NameLookup,
    taken: &HashSet<String>,
) -> Result<String> {
    let mut pass = 0u32;
    let mut modlabel = label.to_owned();
    loop {
        let conname = make_object_name(name1, name2, &modlabel);
        if !taken.contains(&conname) && !lookup.constraint_exists(nsp, &conname)? {
            return Ok(conname);
        }
        pass += 1;
        modlabel = format!("{label}{pass}");
    }
}

/// `ChooseIndexName`: 索引（と、それが所有されるなら制約）の自動名。
/// PRIMARY KEY は `t_pkey`、UNIQUE 制約は `t_a_b_key`、通常の索引は `t_a_b_idx`。
pub fn choose_index_name(
    table: &str,
    nsp: Oid,
    index_column_names: &[String],
    primary: bool,
    is_constraint: bool,
    lookup: &dyn NameLookup,
    taken: &HashSet<String>,
) -> Result<String> {
    if primary {
        return choose_relation_name(table, None, "pkey", nsp, true, lookup, taken);
    }
    let addition = choose_index_name_addition(index_column_names);
    let label = if is_constraint { "key" } else { "idx" };
    choose_relation_name(
        table,
        Some(&addition),
        label,
        nsp,
        is_constraint,
        lookup,
        taken,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 名前の表を持つ偽物。
    #[derive(Default)]
    struct Fake {
        relations: HashSet<String>,
        constraints: HashSet<String>,
    }

    impl Fake {
        fn with(relations: &[&str], constraints: &[&str]) -> Fake {
            Fake {
                relations: relations.iter().map(|s| (*s).to_owned()).collect(),
                constraints: constraints.iter().map(|s| (*s).to_owned()).collect(),
            }
        }
    }

    impl NameLookup for Fake {
        fn relation_exists(&self, _nsp: Oid, name: &str) -> Result<bool> {
            Ok(self.relations.contains(name))
        }
        fn constraint_exists(&self, _nsp: Oid, name: &str) -> Result<bool> {
            Ok(self.constraints.contains(name))
        }
    }

    const NSP: Oid = 2200;

    fn names(cols: &[&str]) -> Vec<String> {
        choose_index_column_names(cols)
    }

    fn idx(
        table: &str,
        cols: &[&str],
        primary: bool,
        constraint: bool,
        f: &Fake,
        taken: &HashSet<String>,
    ) -> String {
        choose_index_name(table, NSP, &names(cols), primary, constraint, f, taken).unwrap()
    }

    #[test]
    fn make_object_name_basics() {
        assert_eq!(make_object_name("t", Some("a"), "check"), "t_a_check");
        assert_eq!(make_object_name("t", None, "pkey"), "t_pkey");
        let long = "a".repeat(80);
        let n = make_object_name(&long, Some("col"), "check");
        assert_eq!(n.len(), 63);
        assert!(n.ends_with("_col_check"));
        assert_eq!(n, format!("{}_col_check", "a".repeat(53)));
    }

    #[test]
    fn make_object_name_shortens_the_longer_name_and_keeps_utf8() {
        // 実測: 60 文字の表名 + 主キー。
        let t = "a".repeat(60);
        let n = make_object_name(&t, None, "pkey");
        assert_eq!(n, format!("{}_pkey", "a".repeat(58)));
        assert_eq!(n.len(), 63);
        // 両方長いときは同じだけ削る（29 文字ずつ）。
        let col = "long_column_name_number_one_long_column_name_number_two";
        let n = make_object_name(&t, Some(col), "key");
        assert_eq!(n.len(), 63);
        assert!(n.starts_with(&format!("{}_long_column_name_number_one_l", "a".repeat(29))));
        // UTF-8 の途中では切らない。
        let jp = "あ".repeat(30); // 90 バイト
        let n = make_object_name(&jp, None, "pkey");
        assert!(n.len() <= 63);
        assert!(n.ends_with("_pkey"));
        assert!(std::str::from_utf8(n.as_bytes()).is_ok());
    }

    #[test]
    fn index_column_names_are_made_unique() {
        assert_eq!(names(&["a", "a"]), ["a", "a1"]);
        assert_eq!(names(&["a", "a1", "a"]), ["a", "a1", "a2"]);
        assert_eq!(names(&["a", "b"]), ["a", "b"]);
        // 元の名前を切り詰めてから数字を付ける。
        let long = "x".repeat(63);
        let r = names(&[&long, &long]);
        assert_eq!(r[0], long);
        assert_eq!(r[1].len(), 63);
        assert!(r[1].ends_with('1'));
    }

    #[test]
    fn index_name_addition_stops_at_the_limit() {
        assert_eq!(choose_index_name_addition(&names(&["a", "b"])), "a_b");
        assert_eq!(choose_index_name_addition(&[]), "");
        let add = choose_index_name_addition(&[
            "long_column_name_number_one".to_owned(),
            "long_column_name_number_two".to_owned(),
            "third_column_name_that_is_long".to_owned(),
        ]);
        assert!(add.len() >= 63 || add.ends_with("long"));
        assert!(add.len() <= 64);
    }

    #[test]
    fn primary_unique_and_plain_index_names() {
        let f = Fake::default();
        let none = HashSet::new();
        assert_eq!(idx("t2", &["a", "b"], true, true, &f, &none), "t2_pkey");
        assert_eq!(idx("t2", &["b", "c"], false, true, &f, &none), "t2_b_c_key");
        assert_eq!(idx("t2", &["a"], false, true, &f, &none), "t2_a_key");
        assert_eq!(idx("t2", &["a"], false, false, &f, &none), "t2_a_idx");
        assert_eq!(
            idx("t2", &["a", "b"], false, false, &f, &none),
            "t2_a_b_idx"
        );
        // create index on e1 (a, a)
        assert_eq!(
            idx("e1", &["a", "a"], false, false, &f, &none),
            "e1_a_a1_idx"
        );
    }

    #[test]
    fn consecutive_indexes_get_numbered_names() {
        // 列・向きが違っても名前は t_a_idx からの連番になる。
        let mut f = Fake::default();
        let none = HashSet::new();
        let mut got = Vec::new();
        for cols in [&["a"][..], &["a"], &["a"], &["a"], &["a", "b"], &["a"]] {
            let n = idx("t2", cols, false, false, &f, &none);
            f.relations.insert(n.clone());
            got.push(n);
        }
        assert_eq!(
            got,
            [
                "t2_a_idx",
                "t2_a_idx1",
                "t2_a_idx2",
                "t2_a_idx3",
                "t2_a_b_idx",
                "t2_a_idx4"
            ]
        );
    }

    #[test]
    fn relation_and_constraint_clashes() {
        let none = HashSet::new();
        // 同じ名前のリレーションがすでにある。
        let f = Fake::with(&["t3_a_key"], &[]);
        assert_eq!(idx("t3", &["a"], false, true, &f, &none), "t3_a_key1");
        // 同じ名前の制約がすでにある（索引と制約は名前を共有する）。
        let f = Fake::with(&[], &["t4_a_key"]);
        assert_eq!(idx("t4", &["a"], false, true, &f, &none), "t4_a_key1");
        // 制約を持たない索引は制約の名前を見ない。
        assert_eq!(idx("t4", &["a"], false, false, &f, &none), "t4_a_idx");
        // 同じ文の中で先に決めた名前。
        let f = Fake::default();
        let taken: HashSet<String> = ["t7_a_key".to_owned()].into_iter().collect();
        assert_eq!(idx("t7", &["a"], false, true, &f, &taken), "t7_a_key1");
        // ALTER TABLE ADD UNIQUE (c) を 2 回。
        let f = Fake::with(&["a1_c_key"], &["a1_c_key"]);
        assert_eq!(idx("a1", &["c"], false, true, &f, &none), "a1_c_key1");
        let f = Fake::with(&["a1_c_key", "a1_c_key1"], &["a1_c_key", "a1_c_key1"]);
        assert_eq!(idx("a1", &["c"], false, true, &f, &none), "a1_c_key2");
    }

    #[test]
    fn constraint_names_look_at_constraints_only() {
        let none = HashSet::new();
        let f = Fake::with(&["t_a_check"], &[]);
        assert_eq!(
            choose_constraint_name("t", Some("a"), "check", NSP, &f, &none).unwrap(),
            "t_a_check"
        );
        let f = Fake::with(&[], &["t_a_check"]);
        assert_eq!(
            choose_constraint_name("t", Some("a"), "check", NSP, &f, &none).unwrap(),
            "t_a_check1"
        );
        let taken: HashSet<String> = ["t_a_check".to_owned(), "t_a_check1".to_owned()]
            .into_iter()
            .collect();
        assert_eq!(
            choose_constraint_name("t", Some("a"), "check", NSP, &Fake::default(), &taken).unwrap(),
            "t_a_check2"
        );
    }

    #[test]
    fn long_names_from_the_measured_table() {
        // 60 文字の表 + primary key (b) + c int unique + unique (long..one, long..two) + create index on t (c)
        let t = "a".repeat(60);
        let mut f = Fake::default();
        let none = HashSet::new();
        let take = |n: String, f: &mut Fake| {
            f.relations.insert(n.clone());
            f.constraints.insert(n.clone());
            n
        };
        let pk = idx(&t, &["b"], true, true, &f, &none);
        assert_eq!(pk, format!("{}_pkey", "a".repeat(58)));
        take(pk, &mut f);
        let ck = idx(&t, &["c"], false, true, &f, &none);
        assert_eq!(ck, format!("{}_c_key", "a".repeat(57)));
        take(ck, &mut f);
        let lk = idx(
            &t,
            &["long_column_name_number_one", "long_column_name_number_two"],
            false,
            true,
            &f,
            &none,
        );
        assert_eq!(
            lk,
            format!("{}_long_column_name_number_one_l_key", "a".repeat(29))
        );
        assert_eq!(lk.len(), 63);
        take(lk, &mut f);
        let ix = idx(&t, &["c"], false, false, &f, &none);
        assert_eq!(ix, format!("{}_c_idx", "a".repeat(57)));
        assert_eq!(ix.len(), 63);
    }

    #[test]
    fn lookup_errors_propagate() {
        struct Broken;
        impl NameLookup for Broken {
            fn relation_exists(&self, _: Oid, _: &str) -> Result<bool> {
                Err(crate::error::Error::internal("boom"))
            }
            fn constraint_exists(&self, _: Oid, _: &str) -> Result<bool> {
                Err(crate::error::Error::internal("boom"))
            }
        }
        let none = HashSet::new();
        assert!(choose_relation_name("t", None, "pkey", NSP, false, &Broken, &none).is_err());
        assert!(choose_constraint_name("t", None, "pkey", NSP, &Broken, &none).is_err());
    }
}
