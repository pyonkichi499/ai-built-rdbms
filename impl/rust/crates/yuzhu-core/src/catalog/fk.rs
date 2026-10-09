//! 外部キー（FK）のカタログの型（`m5/07` §4.2）。
//!
//! 型と `pg_get_constraintdef` の文字列の形だけを持つ。`pg_constraint` の読み書き（`store_fk.rs`）と
//! 実行（`executor/ri/`）は FK-1・FK-2 が書く。

use crate::deparse::ident::quote_identifier;
use crate::types::Oid;

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FkAction {
    NoAction,
    Restrict,
    Cascade,
    SetNull,
    SetDefault,
}

impl FkAction {
    /// `pg_constraint.confupdtype` / `confdeltype`。
    pub fn code(self) -> char {
        match self {
            FkAction::NoAction => 'a',
            FkAction::Restrict => 'r',
            FkAction::Cascade => 'c',
            FkAction::SetNull => 'n',
            FkAction::SetDefault => 'd',
        }
    }

    pub fn from_code(c: char) -> Option<FkAction> {
        Some(match c {
            'a' => FkAction::NoAction,
            'r' => FkAction::Restrict,
            'c' => FkAction::Cascade,
            'n' => FkAction::SetNull,
            'd' => FkAction::SetDefault,
            _ => return None,
        })
    }

    pub fn sql(self) -> &'static str {
        match self {
            FkAction::NoAction => "NO ACTION",
            FkAction::Restrict => "RESTRICT",
            FkAction::Cascade => "CASCADE",
            FkAction::SetNull => "SET NULL",
            FkAction::SetDefault => "SET DEFAULT",
        }
    }
}

/// `pg_constraint.confmatchtype`（`MATCH PARTIAL` は構文の時点で 0A000 にする）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum FkMatch {
    /// `s`.
    Simple,
    /// `f`.
    Full,
}

/// 列ごとの比較演算子（`pg_operator.oid`）。pf = 親キー列 = 子の列、pp = 親 = 親、ff = 子 = 子。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct FkEqOps {
    pub pf: Oid,
    pub pp: Oid,
    pub ff: Oid,
}

/// `pg_constraint` の 1 行（contype `f`）の実行用の写し。
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ForeignKeyDef {
    pub oid: Oid,
    pub name: String,
    /// `conrelid`.
    pub child: Oid,
    /// `confrelid`.
    pub parent: Oid,
    /// `conindid`.
    pub parent_index: Oid,
    /// `conkey`（attnum）。
    pub child_cols: Vec<i16>,
    /// `confkey`（attnum）。
    pub parent_cols: Vec<i16>,
    pub match_type: FkMatch,
    pub on_update: FkAction,
    pub on_delete: FkAction,
    /// `ON DELETE SET NULL / SET DEFAULT (cols)`。空 = FK の全列。attnum は子の表のもの。
    pub set_cols: Vec<i16>,
    pub eq_ops: Vec<FkEqOps>,
}

impl ForeignKeyDef {
    pub fn is_self_referencing(&self) -> bool {
        self.child == self.parent
    }
}

/// `fk_constraint_def` が名前を引くための口。
pub trait FkNames {
    fn column_name(&self, table: Oid, attnum: i16) -> Option<String>;
    /// 検索パスに応じた修飾（`relation_name` と同じ規則）。
    fn relation_name(&self, table: Oid) -> Option<String>;
}

/// `pg_get_constraintdef` の FK の形:
/// `FOREIGN KEY (a, b) REFERENCES t(x, y) [MATCH FULL] [ON UPDATE act] [ON DELETE act [(cols)]]`。
/// `NO ACTION` は省く。名前が引けない列・表は `?` にする。
pub fn fk_constraint_def(fk: &ForeignKeyDef, names: &dyn FkNames) -> String {
    let cols = |table: Oid, attnums: &[i16]| {
        attnums
            .iter()
            .map(|n| {
                names
                    .column_name(table, *n)
                    .map_or_else(|| "?".to_owned(), |c| quote_identifier(&c))
            })
            .collect::<Vec<_>>()
            .join(", ")
    };
    let parent = names
        .relation_name(fk.parent)
        .unwrap_or_else(|| "?".to_owned());
    let mut s = format!(
        "FOREIGN KEY ({}) REFERENCES {parent}({})",
        cols(fk.child, &fk.child_cols),
        cols(fk.parent, &fk.parent_cols)
    );
    if fk.match_type == FkMatch::Full {
        s.push_str(" MATCH FULL");
    }
    if fk.on_update != FkAction::NoAction {
        s.push_str(" ON UPDATE ");
        s.push_str(fk.on_update.sql());
    }
    if fk.on_delete != FkAction::NoAction {
        s.push_str(" ON DELETE ");
        s.push_str(fk.on_delete.sql());
        if matches!(fk.on_delete, FkAction::SetNull | FkAction::SetDefault)
            && !fk.set_cols.is_empty()
        {
            s.push_str(&format!(" ({})", cols(fk.child, &fk.set_cols)));
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    struct N;
    impl FkNames for N {
        fn column_name(&self, table: Oid, attnum: i16) -> Option<String> {
            Some(format!("c{table}_{attnum}"))
        }
        fn relation_name(&self, table: Oid) -> Option<String> {
            Some(format!("t{table}"))
        }
    }

    fn fk() -> ForeignKeyDef {
        ForeignKeyDef {
            oid: 1,
            name: "f".into(),
            child: 10,
            parent: 20,
            parent_index: 30,
            child_cols: vec![1, 2],
            parent_cols: vec![1, 3],
            match_type: FkMatch::Simple,
            on_update: FkAction::NoAction,
            on_delete: FkAction::NoAction,
            set_cols: vec![],
            eq_ops: vec![],
        }
    }

    #[test]
    fn action_codes_round_trip() {
        for a in [
            FkAction::NoAction,
            FkAction::Restrict,
            FkAction::Cascade,
            FkAction::SetNull,
            FkAction::SetDefault,
        ] {
            assert_eq!(FkAction::from_code(a.code()), Some(a));
        }
        assert_eq!(FkAction::from_code('x'), None);
    }

    #[test]
    fn constraint_def_omits_no_action() {
        assert_eq!(
            fk_constraint_def(&fk(), &N),
            "FOREIGN KEY (c10_1, c10_2) REFERENCES t20(c20_1, c20_3)"
        );
        let mut f = fk();
        f.match_type = FkMatch::Full;
        f.on_update = FkAction::Cascade;
        f.on_delete = FkAction::SetNull;
        f.set_cols = vec![2];
        assert_eq!(
            fk_constraint_def(&f, &N),
            "FOREIGN KEY (c10_1, c10_2) REFERENCES t20(c20_1, c20_3) MATCH FULL ON UPDATE CASCADE ON DELETE SET NULL (c10_2)"
        );
        assert!(!f.is_self_referencing());
    }
}
