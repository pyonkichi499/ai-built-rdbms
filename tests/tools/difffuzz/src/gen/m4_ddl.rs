//! ddl 領域: CREATE / DROP INDEX、ALTER TABLE ADD PRIMARY KEY / UNIQUE、TRUNCATE、serial、DROP TABLE の連鎖。
//!
//! 各 DDL の後でカタログ（`pg_class` / `pg_index`）と表の中身を比べる。ALTER TABLE は ADD PRIMARY KEY / UNIQUE だけ（KD-17）。
//! `TRUNCATE ... RESTART IDENTITY` と `ALTER SEQUENCE` はトランザクションの外でだけ使う（KD-11）。

use super::m4::{self, MTable, MTy};
use super::m4_index::{create_index_sql, index_cols};
use super::Ctx;

fn catalog_checks(ctx: &mut Ctx, t: &MTable) {
    let p = ctx.prefix.replace('_', "\\_");
    ctx.push_q(
        format!(
            "SELECT relname, relkind FROM pg_class WHERE relname LIKE '{p}%' AND relkind IN ('r', 'i', 'S') ORDER BY 1, 2;"
        ),
        true,
    );
    ctx.push_q(
        format!(
            "SELECT c.relname, i.indisunique, i.indisprimary FROM pg_index i JOIN pg_class c ON c.oid = i.indexrelid WHERE i.indrelid = '{}'::regclass ORDER BY 1;",
            t.name
        ),
        true,
    );
}

fn table_rows(t: &MTable) -> String {
    format!("SELECT * FROM {} ORDER BY {};", t.name, t.order_all())
}

pub fn scenario(ctx: &mut Ctx) {
    // 表: serial の k（主キーあり / なし）、またはふつうの k
    let name = ctx.fresh_table_name();
    let mut cols = vec![("k".to_string(), MTy::Int4), ("a".to_string(), MTy::Int4)];
    for (n, t) in m4::COLS.iter().skip(2) {
        if ctx.rng.chance(40) {
            cols.push((n.to_string(), *t));
        }
    }
    let serial = ctx.rng.chance(60);
    let pk = if ctx.rng.chance(40) {
        vec!["k".to_string()]
    } else {
        Vec::new()
    };
    let t = MTable {
        name,
        cols,
        pk,
        uniq: Vec::new(),
        serial,
    };
    let sql = m4::create_sql(ctx, &t);
    ctx.push(sql);
    let mut exists = true;
    let mut has_pk = !t.pk.is_empty();
    let mut idx_names: Vec<String> = Vec::new();
    let mut n_idx = 0;
    let n = ctx.rng.range(3, 6) as usize;
    let s = m4::insert_sql(ctx, &t, n, true, 15);
    ctx.push(s);
    for _ in 0..ctx.rng.range(8, 14) {
        if !exists {
            // DROP 後: 存在しない表への操作（42P01）のあと作り直す
            let s = match ctx.rng.below(3) {
                0 => format!("SELECT * FROM {};", t.name),
                1 => format!("TRUNCATE {};", t.name),
                _ => format!("CREATE INDEX ON {} (a);", t.name),
            };
            if s.starts_with("SELECT") {
                ctx.push_q(s, false);
            } else {
                ctx.push(s);
            }
            let sql = m4::create_sql(ctx, &t);
            ctx.push(sql);
            exists = true;
            has_pk = !t.pk.is_empty();
            idx_names.clear();
            continue;
        }
        let r = ctx.rng.below(100);
        if r < 18 {
            let cols = index_cols(&mut ctx.rng, &t, 3);
            let unique = ctx.rng.chance(25);
            let named = ctx.rng.chance(75);
            let name = format!("{}ix{n_idx}", ctx.prefix);
            n_idx += 1;
            let s = create_index_sql(
                &mut ctx.rng,
                &t,
                if named { Some(&name) } else { None },
                unique,
                &cols,
            );
            ctx.push(s);
            if named {
                idx_names.push(name);
            }
            catalog_checks(ctx, &t);
        } else if r < 25 && !idx_names.is_empty() {
            // 既存の名前での CREATE INDEX（42P07）
            let name = ctx.rng.pick(&idx_names).clone();
            ctx.push(format!("CREATE INDEX {name} ON {} (a);", t.name));
        } else if r < 38 {
            let s = match ctx.rng.below(5) {
                0 if !idx_names.is_empty() => {
                    let i = ctx.rng.below(idx_names.len() as u64) as usize;
                    let nm = idx_names.remove(i);
                    format!("DROP INDEX {nm};")
                }
                1 if !idx_names.is_empty() => {
                    let i = ctx.rng.below(idx_names.len() as u64) as usize;
                    let nm = idx_names.remove(i);
                    format!("DROP INDEX IF EXISTS {nm};")
                }
                2 => format!("DROP INDEX IF EXISTS {}nosuch;", ctx.prefix),
                3 => format!("DROP INDEX {}nosuch;", ctx.prefix),
                _ if has_pk => format!("DROP INDEX {}_pkey;", t.name),
                _ => format!("DROP INDEX IF EXISTS {}_pkey;", t.name),
            };
            ctx.push(s);
            catalog_checks(ctx, &t);
        } else if r < 52 {
            // ALTER TABLE ADD PRIMARY KEY / UNIQUE
            let s = match ctx.rng.below(4) {
                0 => format!("ALTER TABLE {} ADD PRIMARY KEY (k);", t.name),
                1 => format!("ALTER TABLE {} ADD PRIMARY KEY (k, a);", t.name),
                2 => format!("ALTER TABLE {} ADD UNIQUE (a);", t.name),
                _ => format!(
                    "ALTER TABLE {} ADD CONSTRAINT {}u{n_idx} UNIQUE (a, k);",
                    t.name, ctx.prefix
                ),
            };
            n_idx += 1;
            ctx.push(s.clone());
            if s.contains("PRIMARY KEY") && !has_pk {
                // 成功したかどうかは両者の応答で比べる。成功時の PRIMARY KEY の有無は pg_index で確かめる
                has_pk = true;
            }
            catalog_checks(ctx, &t);
            // 追加した制約が効くか: 重複の INSERT
            let s = m4::insert_sql(ctx, &t, 2, false, 15);
            ctx.push(s);
            ctx.push_q(table_rows(&t), true);
        } else if r < 64 {
            let s = match ctx.rng.below(5) {
                0 => format!("TRUNCATE {};", t.name),
                1 => format!("TRUNCATE TABLE {} RESTART IDENTITY;", t.name),
                2 => format!("TRUNCATE {} CONTINUE IDENTITY;", t.name),
                3 => format!("TRUNCATE TABLE ONLY {} CASCADE;", t.name),
                _ => format!("TRUNCATE {} RESTRICT;", t.name),
            };
            ctx.push(s);
            ctx.push_q(table_rows(&t), true);
            // serial の続き
            let s = m4::insert_sql(ctx, &t, 3, true, 15);
            ctx.push(s);
            ctx.push_q(table_rows(&t), true);
        } else if r < 84 {
            // DML（索引と制約の維持）
            let s = match ctx.rng.below(4) {
                0 => m4::insert_sql(ctx, &t, 2, false, 15),
                1 => format!(
                    "DELETE FROM {} AS x WHERE {};",
                    t.name,
                    m4::pred(&mut ctx.rng, "x", &t, 1)
                ),
                2 => {
                    let v = ctx.rng.range(0, 6);
                    format!(
                        "UPDATE {} SET a = {v} WHERE k = {};",
                        t.name,
                        ctx.rng.range(0, 6)
                    )
                }
                _ => format!(
                    "INSERT INTO {} (a) VALUES ({}), (DEFAULT);",
                    t.name,
                    ctx.rng.range(0, 6)
                ),
            };
            ctx.push(s);
            ctx.push_q(table_rows(&t), true);
        } else if r < 92 {
            // DROP TABLE の連鎖（複数・IF EXISTS・CASCADE / RESTRICT）。作り直しで serial が 1 から始まることも確かめる
            let s = match ctx.rng.below(5) {
                0 => format!("DROP TABLE {};", t.name),
                1 => format!("DROP TABLE IF EXISTS {}, {}nosuch;", t.name, ctx.prefix),
                2 => format!("DROP TABLE {} CASCADE;", t.name),
                3 => format!("DROP TABLE {} RESTRICT;", t.name),
                _ => format!("DROP TABLE {}, {}nosuch;", t.name, ctx.prefix),
            };
            // 2 番目が存在しない表なら全体が失敗して表は残る
            let fails = s.contains("nosuch;") && !s.contains("IF EXISTS");
            ctx.push(s);
            if !fails {
                exists = false;
            }
            let p = ctx.prefix.replace('_', "\\_");
            ctx.push_q(
                format!("SELECT relname, relkind FROM pg_class WHERE relname LIKE '{p}%' AND relkind IN ('r', 'i', 'S') ORDER BY 1, 2;"),
                true,
            );
        } else {
            catalog_checks(ctx, &t);
        }
    }
    if exists {
        ctx.push_q(table_rows(&t), true);
    }
}
