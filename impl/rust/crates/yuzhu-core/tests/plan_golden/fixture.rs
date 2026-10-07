//! `plan_golden` の足場: `-- schema` の DDL から表と索引を作る（`m4/04` §11.1）。
//!
//! 実体は `yuzhu_core::planner::rules::testutil::Fixture`（実パーサ + 解析。ストレージなし。`nblocks` は偽物）。

use yuzhu_core::planner::rules::testutil::Fixture;

/// `schema`（`CREATE TABLE` / `CREATE [UNIQUE] INDEX` の列）と `nblocks`（`表 = 値` の行）から作る。
pub(crate) fn build(schema: &str, nblocks: &[(String, u32)]) -> Result<Fixture, String> {
    let f =
        Fixture::new(schema).map_err(|e| format!("schema: {} {}", e.sqlstate.code(), e.message))?;
    for (t, n) in nblocks {
        f.set_nblocks(t, *n)
            .map_err(|e| format!("nblocks {t}: {}", e.message))?;
    }
    Ok(f)
}

/// build と各ルールの後の論理プランの印字（`(段階名, 印字)`）。エラーは `ERROR <sqlstate> <message>`。
pub(crate) fn logical_stages(f: &Fixture, sql: &str) -> Vec<(String, String)> {
    match f.stages(sql) {
        Ok(v) => v,
        Err(e) => vec![(
            "error".to_owned(),
            format!("{} {}\n", e.sqlstate.code(), e.message),
        )],
    }
}
