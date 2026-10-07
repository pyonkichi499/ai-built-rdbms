//! WITH 句（CTE）。`m4/03-parser-analyzer.md` §3.3・§4.6・§5.9（N3）。
//!
//! `analyze_query`（`select.rs`）が [`Analyzer::analyze_with`] を呼び、`from.rs` が
//! [`Analyzer::find_cte`] / [`Analyzer::is_future_cte`] を呼ぶ。

use std::cell::RefCell;

use super::Analyzer;
use super::bound::{BoundCte, CteMaterialize, RteColumn};
use super::scope::QueryEnv;
use crate::error::{Error, Result, sqlstate};
use crate::expr::CteId;
use crate::sql::ast::With;
use crate::types::Oid;

/// `BoundQuery` の入れ子と 1 対 1 の連鎖（WITH がなくても 1 段作る。`CteRef.levels_up` を数えるため）。
#[derive(Debug)]
pub(super) struct CteScope<'a> {
    pub(super) parent: Option<&'a CteScope<'a>>,
    /// 解析済み（参照できる）項目。1 項目ずつ足す。
    pub(super) visible: RefCell<Vec<CteEntry>>,
    /// この WITH の全項目名（前方参照の診断と RECURSIVE の判定に使う）。
    pub(super) all_names: Vec<String>,
    pub(super) recursive: bool,
}

#[derive(Debug, Clone)]
pub(super) struct CteEntry {
    pub(super) name: String,
    pub(super) id: CteId,
    /// 別名適用後の出力列（名前と型）。
    pub(super) columns: Vec<RteColumn>,
    /// 出力列それぞれの由来（`(table_oid, attnum)`）。
    pub(super) origins: Vec<(Oid, i16)>,
}

impl<'a> CteScope<'a> {
    /// 根（WITH を持つ外側がない）。
    pub(super) fn root() -> Self {
        CteScope {
            parent: None,
            visible: RefCell::default(),
            all_names: Vec::new(),
            recursive: false,
        }
    }

    /// `parent` の 1 段内側（WITH がない `BoundQuery` もこれを 1 段作る）。
    pub(super) fn level(parent: &'a CteScope<'a>) -> Self {
        CteScope {
            parent: Some(parent),
            ..CteScope::root()
        }
    }
}

#[derive(Debug)]
pub(super) enum CteLookup {
    /// `levels_up` は CTE を宣言した `BoundQuery` までの入れ子の深さ。
    Found {
        levels_up: u16,
        entry: CteEntry,
    },
    /// 再帰の WITH で宣言済みだが未解析（自己・前方）。0A000。
    Recursive,
    NotFound,
}

/// `analyze_with` の結果。`scope` は本体の `BoundQuery` 用の CTE スコープ。
#[derive(Debug)]
pub(super) struct WithResult<'e> {
    pub(super) ctes: Vec<BoundCte>,
    pub(super) scope: CteScope<'e>,
}

impl Analyzer<'_> {
    /// WITH の各項目を宣言順に 1 つずつ解析する（`m4/03` §5.9.1、§4.6）。
    ///
    /// 項目の本体は、この WITH の 1 段内側の問い合わせ（`analyze_query` が `CteScope::level` を作る）として
    /// 解析する。解析の済んだ項目だけが `scope.visible` に入るので、自己参照・前方参照は見つからない。
    pub(super) fn analyze_with<'e>(
        &self,
        with: &With,
        env: &QueryEnv<'e>,
    ) -> Result<WithResult<'e>> {
        let mut all_names: Vec<String> = Vec::with_capacity(with.ctes.len());
        for cte in &with.ctes {
            if all_names.contains(&cte.name.value) {
                return Err(Error::new(
                    sqlstate::DUPLICATE_ALIAS,
                    format!(
                        "WITH query name \"{}\" specified more than once",
                        cte.name.value
                    ),
                )
                .with_span(cte.name.span));
            }
            all_names.push(cte.name.value.clone());
        }
        let scope = CteScope {
            parent: Some(env.ctes),
            visible: RefCell::default(),
            all_names,
            recursive: with.recursive,
        };
        let mut ctes = Vec::with_capacity(with.ctes.len());
        for (i, cte) in with.ctes.iter().enumerate() {
            let body_env = QueryEnv {
                outer: env.outer,
                ctes: &scope,
                resolve_unknowns: true,
            };
            let query = self.analyze_query(&cte.query, &body_env)?;
            if cte.columns.len() > query.columns.len() {
                return Err(Error::new(
                    sqlstate::INVALID_COLUMN_REFERENCE,
                    format!(
                        "WITH query \"{}\" has {} columns available but {} columns specified",
                        cte.name.value,
                        query.columns.len(),
                        cte.columns.len()
                    ),
                )
                .with_span(cte.name.span));
            }
            let columns: Vec<RteColumn> = query
                .columns
                .iter()
                .enumerate()
                .map(|(j, c)| RteColumn {
                    name: cte
                        .columns
                        .get(j)
                        .map_or_else(|| c.name.clone(), |a| a.value.clone()),
                    ty: c.ty,
                })
                .collect();
            let origins = query
                .columns
                .iter()
                .map(|c| (c.table_oid, c.attnum))
                .collect();
            let id = u16::try_from(i).map(CteId).map_err(|_| {
                Error::new(sqlstate::PROGRAM_LIMIT_EXCEEDED, "too many WITH queries")
                    .with_span(cte.name.span)
            })?;
            scope.visible.borrow_mut().push(CteEntry {
                name: cte.name.value.clone(),
                id,
                columns,
                origins,
            });
            ctes.push(BoundCte {
                name: cte.name.value.clone(),
                query,
                materialize: match cte.materialized {
                    Some(true) => CteMaterialize::Always,
                    Some(false) => CteMaterialize::Never,
                    None => CteMaterialize::Default,
                },
                col_aliases: cte.columns.iter().map(|c| c.value.clone()).collect(),
            });
        }
        Ok(WithResult { ctes, scope })
    }

    /// FROM 句の表名（スキーマなし）を CTE として探す（`m4/03` §5.9.2 の 1・2）。
    ///
    /// 内側の段から順に、解析済みの項目だけを見る（最も内側の宣言が勝つ）。見つからず、どこかの段が
    /// `RECURSIVE` でその WITH の項目名（未解析）に同名があれば `Recursive`（呼び出し側が 0A000）。
    #[allow(clippy::unused_self)]
    pub(super) fn find_cte(&self, name: &str, ctes: &CteScope<'_>) -> CteLookup {
        let mut levels: u16 = 0;
        let mut cur = Some(ctes);
        while let Some(s) = cur {
            if let Some(entry) = s.visible.borrow().iter().find(|e| e.name == name) {
                return CteLookup::Found {
                    levels_up: levels,
                    entry: entry.clone(),
                };
            }
            cur = s.parent;
            levels = levels.saturating_add(1);
        }
        let mut cur = Some(ctes);
        while let Some(s) = cur {
            if s.recursive && s.all_names.iter().any(|n| n == name) {
                return CteLookup::Recursive;
            }
            cur = s.parent;
        }
        CteLookup::NotFound
    }

    /// 同じ WITH の中の、まだ参照できない項目か（42P01 の DETAIL / HINT 用。`m4/03` §5.9.2 の 4）。
    #[allow(clippy::unused_self)]
    pub(super) fn is_future_cte(&self, name: &str, ctes: &CteScope<'_>) -> bool {
        let mut cur = Some(ctes);
        while let Some(s) = cur {
            if s.all_names.iter().any(|n| n == name)
                && !s.visible.borrow().iter().any(|e| e.name == name)
            {
                return true;
            }
            cur = s.parent;
        }
        false
    }
}
