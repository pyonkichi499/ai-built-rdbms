//! FOREIGN KEY の構文解析（`m5/07-foreign-key.md` §4.1）。
//!
//! `REFERENCES t [(cols)]` と `FOREIGN KEY (cols) REFERENCES ...` の後ろの句を、gram.y と同じ固定の順序で読む。
//!
//! ```text
//! REFERENCES t [(cols)] [MATCH {SIMPLE | FULL | PARTIAL}]
//!     [ON UPDATE action] [ON DELETE action]    -- 各 1 回まで、順序は自由
//!     [attributes]                             -- [NOT] DEFERRABLE / INITIALLY {IMMEDIATE | DEFERRED} / NOT VALID
//! ```
//!
//! `MATCH PARTIAL` と、`ON UPDATE` の `SET NULL (cols)` / `SET DEFAULT (cols)` はここで `0A000` にする。
//! `DEFERRABLE` / `INITIALLY DEFERRED` / `NOT VALID` は構文としては読み、`0A000` は解析（`bind_foreign_key`）が返す。

use super::Parser;
use crate::error::{Error, Result, sqlstate};
use crate::sql::ast::{FkActionSpec, FkMatchSpec, ForeignKeySpec, Ident, TableConstraintKind};
use crate::sql::token::TokenKind;

impl Parser<'_> {
    /// 列制約の `REFERENCES ...`（カーソルは `REFERENCES` の上）。`start` は制約（`CONSTRAINT n` を含む）の先頭、
    /// `name` は `CONSTRAINT n`。`columns` は空で返す（呼び出し側が定義中の列を入れる）。
    pub(super) fn parse_column_references(
        &mut self,
        start: u32,
        name: Option<Ident>,
    ) -> Result<ForeignKeySpec> {
        self.expect_kw("references")?;
        self.parse_references_tail(start, name, Vec::new(), false)
    }

    /// 表制約の `FOREIGN KEY (cols) REFERENCES ...`（カーソルは `FOREIGN` の上）。
    pub(super) fn parse_table_foreign_key(
        &mut self,
        start: u32,
        name: Option<Ident>,
    ) -> Result<TableConstraintKind> {
        self.expect_kw("foreign")?;
        self.expect_kw("key")?;
        let columns = self.parse_paren_name_list()?;
        self.expect_kw("references")?;
        let spec = self.parse_references_tail(start, name, columns, true)?;
        Ok(TableConstraintKind::ForeignKey(spec))
    }

    /// `REFERENCES` の後ろ: `t [(cols)] [MATCH ...] [ON ...] [attributes]`。
    fn parse_references_tail(
        &mut self,
        start: u32,
        name: Option<Ident>,
        columns: Vec<Ident>,
        table_level: bool,
    ) -> Result<ForeignKeySpec> {
        let ref_table = self.parse_object_name()?;
        let ref_columns = if self.peek_kind() == &TokenKind::LParen {
            self.parse_paren_name_list()?
        } else {
            Vec::new()
        };
        let match_type = self.parse_fk_match()?;
        let (on_update, on_delete) = self.parse_fk_actions()?;
        let attrs = self.parse_fk_attributes(table_level)?;
        Ok(ForeignKeySpec {
            name,
            columns,
            ref_table,
            ref_columns,
            match_type,
            on_update,
            on_delete,
            deferrable: attrs.deferrable,
            initially_deferred: attrs.initially_deferred,
            not_valid: attrs.not_valid,
            span: self.span_from(start),
        })
    }

    /// `[MATCH FULL | PARTIAL | SIMPLE]`。`PARTIAL` は PostgreSQL と同じ `0A000`（位置は `MATCH`）。
    fn parse_fk_match(&mut self) -> Result<FkMatchSpec> {
        if !self.is_kw("match") {
            return Ok(FkMatchSpec::Simple);
        }
        let match_span = self.advance().span;
        if self.eat_kw("full") {
            Ok(FkMatchSpec::Full)
        } else if self.eat_kw("simple") {
            Ok(FkMatchSpec::Simple)
        } else if self.is_kw("partial") {
            Err(Error::not_supported("MATCH PARTIAL not yet implemented").with_span(match_span))
        } else {
            Err(self.unexpected())
        }
    }

    /// `[ON UPDATE a] [ON DELETE a]`（順序は自由、各 1 回まで）。戻り値は `(on_update, on_delete)`。
    /// 同じ側の 2 回目は、gram.y と同じく 2 つ目の `UPDATE` / `DELETE` の位置の構文エラーになる。
    fn parse_fk_actions(&mut self) -> Result<(FkActionSpec, FkActionSpec)> {
        let mut on_update: Option<FkActionSpec> = None;
        let mut on_delete: Option<FkActionSpec> = None;
        while self.is_kw("on") && (on_update.is_none() || on_delete.is_none()) {
            let on_span = self.advance().span;
            // 先に書かれた側は、もう一方のキーワードしか受け付けない。
            let is_update = if on_update.is_none() && on_delete.is_none() {
                if self.eat_kw("update") {
                    true
                } else {
                    self.expect_kw("delete")?;
                    false
                }
            } else if on_update.is_none() {
                self.expect_kw("update")?;
                true
            } else {
                self.expect_kw("delete")?;
                false
            };
            let action = self.parse_fk_action(is_update, on_span)?;
            if is_update {
                on_update = Some(action);
            } else {
                on_delete = Some(action);
            }
        }
        Ok((
            on_update.unwrap_or(FkActionSpec::NoAction),
            on_delete.unwrap_or(FkActionSpec::NoAction),
        ))
    }

    /// `NO ACTION | RESTRICT | CASCADE | SET NULL [(cols)] | SET DEFAULT [(cols)]`。
    fn parse_fk_action(
        &mut self,
        is_update: bool,
        on_span: crate::error::Span,
    ) -> Result<FkActionSpec> {
        if self.eat_kw("no") {
            self.expect_kw("action")?;
            return Ok(FkActionSpec::NoAction);
        }
        if self.eat_kw("restrict") {
            return Ok(FkActionSpec::Restrict);
        }
        if self.eat_kw("cascade") {
            return Ok(FkActionSpec::Cascade);
        }
        self.expect_kw("set")?;
        let is_null = if self.eat_kw("null") {
            true
        } else {
            self.expect_kw("default")?;
            false
        };
        let cols = if self.peek_kind() == &TokenKind::LParen {
            self.parse_paren_name_list()?
        } else {
            Vec::new()
        };
        if is_update && !cols.is_empty() {
            let what = if is_null { "SET NULL" } else { "SET DEFAULT" };
            return Err(Error::new(
                sqlstate::FEATURE_NOT_SUPPORTED,
                format!("a column list with {what} is only supported for ON DELETE actions"),
            )
            .with_span(on_span));
        }
        Ok(if is_null {
            FkActionSpec::SetNull(cols)
        } else {
            FkActionSpec::SetDefault(cols)
        })
    }

    /// 制約の属性: `[NOT] DEFERRABLE`、`INITIALLY {IMMEDIATE | DEFERRED}`、（表制約だけ）`NOT VALID`。
    /// 列制約の `NOT` は次の `NOT NULL` と区別するため 2 トークン先読みし、`NOT DEFERRABLE` のときだけ読む。
    /// 同じ種類を 2 回書くと 42601（gram.y の `processCASbits` と同じ文言）。
    fn parse_fk_attributes(&mut self, table_level: bool) -> Result<FkAttributes> {
        let mut attrs = FkAttributes::default();
        loop {
            let span = self.peek().span;
            if self.eat_kw("deferrable") {
                Self::set_once(
                    &mut attrs.deferrable,
                    true,
                    "DEFERRABLE/NOT DEFERRABLE",
                    span,
                )?;
            } else if self.is_kw("not") && self.nth_is_kw(1, "deferrable") {
                self.advance();
                self.advance();
                Self::set_once(
                    &mut attrs.deferrable,
                    false,
                    "DEFERRABLE/NOT DEFERRABLE",
                    span,
                )?;
            } else if self.eat_kw("initially") {
                let deferred = if self.eat_kw("deferred") {
                    true
                } else {
                    self.expect_kw("immediate")?;
                    false
                };
                Self::set_once(
                    &mut attrs.initially_deferred,
                    deferred,
                    "INITIALLY IMMEDIATE/DEFERRED",
                    span,
                )?;
            } else if table_level && self.is_kw("not") && self.nth_is_kw(1, "valid") {
                self.advance();
                self.advance();
                attrs.not_valid = true;
            } else {
                return Ok(attrs);
            }
        }
    }

    fn set_once(
        slot: &mut Option<bool>,
        value: bool,
        what: &str,
        span: crate::error::Span,
    ) -> Result<()> {
        if slot.is_some() {
            return Err(Error::new(
                sqlstate::SYNTAX_ERROR,
                format!("multiple {what} clauses not allowed"),
            )
            .with_span(span));
        }
        *slot = Some(value);
        Ok(())
    }
}

#[derive(Default)]
struct FkAttributes {
    deferrable: Option<bool>,
    initially_deferred: Option<bool>,
    not_valid: bool,
}

#[cfg(test)]
mod tests {
    use crate::error::sqlstate;
    use crate::sql::ast::{
        ColumnConstraintKind, FkActionSpec, FkMatchSpec, ForeignKeySpec, Statement,
        TableConstraintKind, TableElement,
    };

    fn parse(sql: &str) -> crate::error::Result<Vec<Statement>> {
        crate::sql::parse(sql)
    }

    /// CREATE TABLE の最後の要素の FK（表制約）。
    fn table_fk(clause: &str) -> ForeignKeySpec {
        let sql = format!("CREATE TABLE c (a int, b int, {clause})");
        let Statement::CreateTable(ct) = parse(&sql).unwrap().remove(0) else {
            panic!("not CREATE TABLE");
        };
        let Some(TableElement::Constraint(tc)) = ct.elements.into_iter().last() else {
            panic!("no table constraint");
        };
        let TableConstraintKind::ForeignKey(spec) = tc.kind else {
            panic!("not a foreign key");
        };
        spec
    }

    fn col_fk(clause: &str) -> ForeignKeySpec {
        let sql = format!("CREATE TABLE c (a int {clause}, b int)");
        let Statement::CreateTable(ct) = parse(&sql).unwrap().remove(0) else {
            panic!("not CREATE TABLE");
        };
        let TableElement::Column(cd) = &ct.elements[0] else {
            panic!("not a column");
        };
        let ColumnConstraintKind::References(spec) = &cd.constraints[0].kind else {
            panic!("not REFERENCES: {:?}", cd.constraints);
        };
        spec.clone()
    }

    fn err(sql: &str) -> crate::error::Error {
        parse(sql).unwrap_err()
    }

    #[test]
    fn minimal_column_reference() {
        let s = col_fk("REFERENCES p");
        assert_eq!(s.columns.len(), 1);
        assert_eq!(s.columns[0].value, "a");
        assert_eq!(s.ref_table.name().value, "p");
        assert!(s.ref_columns.is_empty());
        assert_eq!(s.match_type, FkMatchSpec::Simple);
        assert_eq!(s.on_update, FkActionSpec::NoAction);
        assert_eq!(s.on_delete, FkActionSpec::NoAction);
        assert_eq!(s.deferrable, None);
        assert_eq!(s.initially_deferred, None);
        assert!(!s.not_valid);
        assert!(s.name.is_none());
    }

    #[test]
    fn named_column_reference_with_columns_and_schema() {
        let s = col_fk("CONSTRAINT fk1 REFERENCES s.p (id)");
        assert_eq!(s.name.unwrap().value, "fk1");
        assert_eq!(s.ref_table.parts.len(), 2);
        assert_eq!(s.ref_columns[0].value, "id");
    }

    #[test]
    fn table_foreign_key_full_form() {
        let s = table_fk(
            "CONSTRAINT x FOREIGN KEY (a, b) REFERENCES p (id, k) MATCH FULL \
             ON DELETE CASCADE ON UPDATE SET NULL",
        );
        assert_eq!(s.name.as_ref().unwrap().value, "x");
        assert_eq!(s.columns.len(), 2);
        assert_eq!(s.ref_columns.len(), 2);
        assert_eq!(s.match_type, FkMatchSpec::Full);
        assert_eq!(s.on_delete, FkActionSpec::Cascade);
        assert_eq!(s.on_update, FkActionSpec::SetNull(Vec::new()));
    }

    #[test]
    fn five_actions_both_sides() {
        for (text, want) in [
            ("NO ACTION", FkActionSpec::NoAction),
            ("RESTRICT", FkActionSpec::Restrict),
            ("CASCADE", FkActionSpec::Cascade),
            ("SET NULL", FkActionSpec::SetNull(Vec::new())),
            ("SET DEFAULT", FkActionSpec::SetDefault(Vec::new())),
        ] {
            let s = table_fk(&format!(
                "FOREIGN KEY (a) REFERENCES p ON DELETE {text} ON UPDATE {text}"
            ));
            assert_eq!(s.on_delete, want, "{text}");
            assert_eq!(s.on_update, want, "{text}");
        }
    }

    #[test]
    fn on_update_before_delete_is_the_same() {
        let a = table_fk("FOREIGN KEY (a) REFERENCES p ON UPDATE CASCADE ON DELETE RESTRICT");
        assert_eq!(a.on_update, FkActionSpec::Cascade);
        assert_eq!(a.on_delete, FkActionSpec::Restrict);
    }

    #[test]
    fn set_columns_on_delete() {
        let s = table_fk("FOREIGN KEY (a, b) REFERENCES p ON DELETE SET NULL (b)");
        let FkActionSpec::SetNull(cols) = s.on_delete else {
            panic!("not SET NULL");
        };
        assert_eq!(cols.len(), 1);
        assert_eq!(cols[0].value, "b");
        let s = table_fk("FOREIGN KEY (a, b) REFERENCES p ON DELETE SET DEFAULT (a, b)");
        assert!(matches!(s.on_delete, FkActionSpec::SetDefault(c) if c.len() == 2));
    }

    #[test]
    fn set_columns_on_update_is_0a000() {
        for (what, text) in [("SET NULL", "SET NULL"), ("SET DEFAULT", "SET DEFAULT")] {
            let e = err(&format!(
                "CREATE TABLE c (a int, FOREIGN KEY (a) REFERENCES p ON UPDATE {text} (a))"
            ));
            assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
            assert_eq!(
                e.message,
                format!("a column list with {what} is only supported for ON DELETE actions")
            );
        }
    }

    #[test]
    fn match_partial_is_0a000() {
        let e = err("CREATE TABLE c (a int REFERENCES p MATCH PARTIAL)");
        assert_eq!(e.sqlstate, sqlstate::FEATURE_NOT_SUPPORTED);
        assert_eq!(e.message, "MATCH PARTIAL not yet implemented");
    }

    #[test]
    fn match_simple_and_full() {
        assert_eq!(
            col_fk("REFERENCES p MATCH SIMPLE").match_type,
            FkMatchSpec::Simple
        );
        assert_eq!(
            col_fk("REFERENCES p MATCH FULL").match_type,
            FkMatchSpec::Full
        );
        assert_eq!(
            err("CREATE TABLE c (a int REFERENCES p MATCH x)").sqlstate,
            sqlstate::SYNTAX_ERROR
        );
    }

    #[test]
    fn wrong_order_is_syntax_error_at_match() {
        let e = err("CREATE TABLE c (a int REFERENCES p ON DELETE CASCADE MATCH FULL)");
        assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR);
        assert_eq!(e.message, "syntax error at or near \"MATCH\"");
    }

    #[test]
    fn duplicate_action_is_syntax_error_at_second_keyword() {
        let e = err("CREATE TABLE c (a int REFERENCES p ON DELETE CASCADE ON DELETE RESTRICT)");
        assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR);
        assert_eq!(e.message, "syntax error at or near \"DELETE\"");
        let e = err("CREATE TABLE c (a int REFERENCES p ON UPDATE CASCADE ON UPDATE RESTRICT)");
        assert_eq!(e.message, "syntax error at or near \"UPDATE\"");
    }

    #[test]
    fn third_on_clause_is_syntax_error() {
        let e = err(
            "CREATE TABLE c (a int REFERENCES p ON UPDATE CASCADE ON DELETE CASCADE ON DELETE CASCADE)",
        );
        assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR);
        assert_eq!(e.message, "syntax error at or near \"ON\"");
    }

    #[test]
    fn attributes_are_recorded() {
        let s = col_fk("REFERENCES p DEFERRABLE");
        assert_eq!(s.deferrable, Some(true));
        let s = col_fk("REFERENCES p NOT DEFERRABLE INITIALLY IMMEDIATE");
        assert_eq!(s.deferrable, Some(false));
        assert_eq!(s.initially_deferred, Some(false));
        let s = col_fk("REFERENCES p INITIALLY DEFERRED");
        assert_eq!(s.initially_deferred, Some(true));
        let s = table_fk("FOREIGN KEY (a) REFERENCES p DEFERRABLE INITIALLY DEFERRED");
        assert_eq!(s.deferrable, Some(true));
        assert_eq!(s.initially_deferred, Some(true));
    }

    #[test]
    fn not_valid_only_at_table_level() {
        assert!(table_fk("FOREIGN KEY (a) REFERENCES p NOT VALID").not_valid);
        assert_eq!(
            err("CREATE TABLE c (a int REFERENCES p NOT VALID)").sqlstate,
            sqlstate::SYNTAX_ERROR
        );
    }

    #[test]
    fn duplicate_attributes_are_42601() {
        let e = err("CREATE TABLE c (a int REFERENCES p DEFERRABLE NOT DEFERRABLE)");
        assert_eq!(e.sqlstate, sqlstate::SYNTAX_ERROR);
        assert_eq!(
            e.message,
            "multiple DEFERRABLE/NOT DEFERRABLE clauses not allowed"
        );
        let e = err("CREATE TABLE c (a int REFERENCES p INITIALLY DEFERRED INITIALLY IMMEDIATE)");
        assert_eq!(
            e.message,
            "multiple INITIALLY IMMEDIATE/DEFERRED clauses not allowed"
        );
    }

    #[test]
    fn not_null_after_references_is_a_separate_constraint() {
        let sql = "CREATE TABLE c (a int REFERENCES p NOT NULL, b int)";
        let Statement::CreateTable(ct) = parse(sql).unwrap().remove(0) else {
            panic!();
        };
        let TableElement::Column(cd) = &ct.elements[0] else {
            panic!();
        };
        assert_eq!(cd.constraints.len(), 2);
        assert!(matches!(
            cd.constraints[0].kind,
            ColumnConstraintKind::References(_)
        ));
        assert_eq!(cd.constraints[1].kind, ColumnConstraintKind::NotNull);
        // NOT DEFERRABLE の直後の NOT NULL も読める。
        let sql = "CREATE TABLE c (a int REFERENCES p NOT DEFERRABLE NOT NULL)";
        let Statement::CreateTable(ct) = parse(sql).unwrap().remove(0) else {
            panic!();
        };
        let TableElement::Column(cd) = &ct.elements[0] else {
            panic!();
        };
        assert_eq!(cd.constraints.len(), 2);
    }

    #[test]
    fn column_constraint_name_is_in_spec_and_constraint() {
        let sql = "CREATE TABLE c (a int CONSTRAINT k REFERENCES p)";
        let Statement::CreateTable(ct) = parse(sql).unwrap().remove(0) else {
            panic!();
        };
        let TableElement::Column(cd) = &ct.elements[0] else {
            panic!();
        };
        assert_eq!(cd.constraints[0].name.as_ref().unwrap().value, "k");
        let ColumnConstraintKind::References(spec) = &cd.constraints[0].kind else {
            panic!();
        };
        assert_eq!(spec.name.as_ref().unwrap().value, "k");
    }

    #[test]
    fn deferrable_flag_on_constraint() {
        let sql = "CREATE TABLE c (a int REFERENCES p DEFERRABLE)";
        let Statement::CreateTable(ct) = parse(sql).unwrap().remove(0) else {
            panic!();
        };
        let TableElement::Column(cd) = &ct.elements[0] else {
            panic!();
        };
        assert!(cd.constraints[0].deferrable);
        let sql = "CREATE TABLE c (a int, FOREIGN KEY (a) REFERENCES p INITIALLY DEFERRED)";
        let Statement::CreateTable(ct) = parse(sql).unwrap().remove(0) else {
            panic!();
        };
        let TableElement::Constraint(tc) = &ct.elements[1] else {
            panic!();
        };
        assert!(tc.deferrable);
    }

    #[test]
    fn alter_table_add_foreign_key() {
        let sql = "ALTER TABLE c ADD CONSTRAINT f FOREIGN KEY (a) REFERENCES p (id) \
                   ON DELETE SET NULL NOT VALID";
        let Statement::AlterTable(at) = parse(sql).unwrap().remove(0) else {
            panic!();
        };
        let crate::sql::ast::AlterTableAction::AddConstraint(tc) = at.action else {
            panic!();
        };
        assert_eq!(tc.name.as_ref().unwrap().value, "f");
        let TableConstraintKind::ForeignKey(spec) = tc.kind else {
            panic!();
        };
        assert!(spec.not_valid);
        assert_eq!(spec.on_delete, FkActionSpec::SetNull(Vec::new()));
    }

    #[test]
    fn missing_pieces_are_syntax_errors() {
        for sql in [
            "CREATE TABLE c (a int REFERENCES)",
            "CREATE TABLE c (a int, FOREIGN KEY a REFERENCES p)",
            "CREATE TABLE c (a int, FOREIGN KEY (a) p)",
            "CREATE TABLE c (a int REFERENCES p ON DELETE)",
            "CREATE TABLE c (a int REFERENCES p ON DELETE SET)",
            "CREATE TABLE c (a int REFERENCES p ON x CASCADE)",
        ] {
            assert_eq!(err(sql).sqlstate, sqlstate::SYNTAX_ERROR, "{sql}");
        }
    }
}
