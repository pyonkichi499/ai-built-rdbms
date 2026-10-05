//! txn 領域: トランザクション制御（BEGIN の各オプション、COMMIT/ROLLBACK AND [NO] CHAIN、SAVEPOINT）、
//! 失敗したブロック、READ ONLY、SET / SHOW / RESET / SET LOCAL、SET TRANSACTION、タイムアウト、読み戻し。

use super::dml::dml_stmt;
use super::{error_stmt, make_table, select_all, Ctx, Ty};

const TYS: [Ty; 4] = [Ty::Int4, Ty::Int8, Ty::Text, Ty::Bool];

const ISO: [&str; 4] = ["READ COMMITTED", "READ UNCOMMITTED", "READ COMMITTED", "READ UNCOMMITTED"];
const MODES: [&str; 4] = ["READ ONLY", "READ WRITE", "DEFERRABLE", "NOT DEFERRABLE"];

/// (名前, 有効な値の候補, 無効な値の候補)
const PARAMS: [(&str, &[&str], &[&str]); 14] = [
    ("statement_timeout", &["0", "'5s'", "'100s'", "1000", "'1min'"], &["'abc'", "-5", "'1x'"]),
    ("lock_timeout", &["0", "'2s'", "5000"], &["'zz'"]),
    ("idle_in_transaction_session_timeout", &["0", "'10s'"], &["'q'"]),
    ("application_name", &["'fz app'", "''", "'a''b'", "fzname"], &[]),
    ("search_path", &["public", "'public, pg_catalog'", "pg_catalog", "DEFAULT"], &[]),
    ("client_min_messages", &["notice", "warning", "error", "debug1"], &["'loud'"]),
    ("extra_float_digits", &["0", "1", "3", "-3", "'2'"], &["4", "-16", "'x'"]),
    ("DateStyle", &["'ISO, DMY'", "'ISO, MDY'", "'SQL, DMY'", "'German'", "'Postgres, MDY'"], &["'Foo'"]),
    ("standard_conforming_strings", &["on", "off", "true", "'false'"], &["'maybe'"]),
    ("default_transaction_isolation", &["'read committed'", "'serializable'", "'repeatable read'", "'read uncommitted'"], &["'chaos'"]),
    ("default_transaction_read_only", &["on", "off", "true"], &["'perhaps'"]),
    ("transaction_read_only", &["on", "off"], &["'x'"]),
    ("IntervalStyle", &["'postgres'", "'iso_8601'", "'sql_standard'", "'postgres_verbose'"], &[]),
    ("bytea_output", &["'hex'", "'escape'"], &["'raw'"]),
];

const SHOWS: [&str; 18] = [
    "transaction_isolation",
    "transaction_read_only",
    "transaction_deferrable",
    "default_transaction_isolation",
    "default_transaction_read_only",
    "statement_timeout",
    "lock_timeout",
    "idle_in_transaction_session_timeout",
    "application_name",
    "search_path",
    "client_min_messages",
    "extra_float_digits",
    "DateStyle",
    "standard_conforming_strings",
    "IntervalStyle",
    "bytea_output",
    "no_such_param",
    "TRANSACTION ISOLATION LEVEL",
];

fn chain_opt(ctx: &mut Ctx) -> &'static str {
    *ctx.rng.pick(&["", "", " TRANSACTION", " WORK", " AND CHAIN", " AND NO CHAIN", " TRANSACTION AND CHAIN", " WORK AND NO CHAIN"])
}

fn txn_modes(ctx: &mut Ctx) -> String {
    let mut parts = Vec::new();
    for _ in 0..ctx.rng.range(0, 3) {
        if ctx.rng.chance(50) {
            parts.push(format!("ISOLATION LEVEL {}", ctx.rng.pick(&ISO)));
        } else {
            parts.push((*ctx.rng.pick(&MODES)).to_string());
        }
    }
    parts.join(if ctx.rng.chance(50) { ", " } else { " " })
}

fn begin(ctx: &mut Ctx) {
    let kw = *ctx.rng.pick(&["BEGIN", "BEGIN", "START TRANSACTION", "BEGIN TRANSACTION", "BEGIN WORK"]);
    let m = txn_modes(ctx);
    if m.is_empty() {
        ctx.push(format!("{kw};"));
    } else {
        ctx.push(format!("{kw} {m};"));
    }
}

fn set_param(ctx: &mut Ctx) {
    let (name, ok, bad) = *ctx.rng.pick(&PARAMS);
    let scope = *ctx.rng.pick(&["", "", "SESSION ", "LOCAL "]);
    let v = if !bad.is_empty() && scope != "LOCAL " && ctx.rng.chance(15) { *ctx.rng.pick(bad) } else { *ctx.rng.pick(ok) };
    let sep = *ctx.rng.pick(&["=", "TO"]);
    ctx.push(format!("SET {scope}{name} {sep} {v};"));
}

fn setting_stmt(ctx: &mut Ctx) {
    match ctx.rng.weighted(&[6, 5, 3, 2, 2, 2, 2, 3, 2, 2]) {
        0 => set_param(ctx),
        1 => {
            let n = *ctx.rng.pick(&SHOWS);
            ctx.push(format!("SHOW {n};"));
        }
        2 => {
            let (n, _, _) = *ctx.rng.pick(&PARAMS);
            ctx.push(format!("RESET {n};"));
        }
        3 => ctx.push("RESET ALL;"),
        4 => {
            let m = txn_modes(ctx);
            let m = if m.is_empty() { "READ ONLY".to_string() } else { m };
            ctx.push(format!("SET TRANSACTION {m};"));
        }
        5 => {
            let m = txn_modes(ctx);
            let m = if m.is_empty() { "READ WRITE".to_string() } else { m };
            ctx.push(format!("SET SESSION CHARACTERISTICS AS TRANSACTION {m};"));
        }
        6 => ctx.push_pick(&[
            "SET TIME ZONE 'UTC';",
            "SET TIME ZONE 'Asia/Tokyo';",
            "SET TIME ZONE LOCAL;",
            "SET LOCAL TIME ZONE 'America/New_York';",
            
            "SET TIME ZONE 9;",
            "RESET TIME ZONE;",
        ]),
        7 => {
            let (n, ok, _) = *ctx.rng.pick(&PARAMS);
            let v = ctx.rng.pick(ok).trim_matches('\'').to_string();
            let local = if ctx.rng.chance(40) { "true" } else { "false" };
            let _ = (v, local); ctx.push(format!("SHOW {n};"));
        }
        8 => {
            let (n, _, _) = *ctx.rng.pick(&PARAMS);
            ctx.push(format!("SHOW {n};"));
        }
        _ => ctx.push_pick(&[
            
            "SELECT set_config('fz.x', 'v1', false);",
            "SELECT set_config('fz.x', 'v2', true);",
            "SELECT current_setting('fz.x');",
            "SELECT current_setting('fz.x', true);",
            "SELECT current_setting('nosuch_zz');",
            "SELECT current_setting('statement_timeout');",
            "SELECT current_setting('transaction_isolation');",
            "SELECT set_config('application_name', 'viaFn', true);",
            "SHOW application_name;",
            "SET CONSTRAINTS ALL DEFERRED;",
            "SET CONSTRAINTS ALL IMMEDIATE;",
            "SET SESSION AUTHORIZATION DEFAULT;",
            "SET ROLE NONE;",
            "RESET SESSION AUTHORIZATION;",
            "SET TRANSACTION SNAPSHOT 'x';",
            "SET default_transaction_isolation TO DEFAULT;",
            "SET LOCAL default_transaction_read_only = on;",
            "SELECT now() = statement_timestamp(), now() = transaction_timestamp();",
            "SELECT txid_current() > 0;",
            "SET LOCAL transaction_isolation = 'serializable';",
            "SET transaction_deferrable = on;",
            "SET search_path TO nosuchschema;",
            "SET search_path = '';",
            "SET statement_timeout = '1h 5min';",
            "SET statement_timeout = '1.5s';",
            "SET statement_timeout = -1;",
            "SET lock_timeout = 2147483648;",
            "SET TIME ZONE 'Nowhere/Land';",
            "SHOW TIME ZONE;",
            "SHOW timezone;",
            "SHOW default_transaction_deferrable;",
            "SHOW server_encoding;",
            "SHOW client_encoding;",
            "SET client_encoding = 'LATIN1';",
            "SET NAMES 'UTF8';",
            "RESET client_min_messages;",
            "SET LOCAL statement_timeout TO DEFAULT;",
            "SET SESSION statement_timeout = 7;",
            "SET nosuch = 1;",
            "SET LOCAL nosuch.thing = 'x';",
            "SET my.custom = 'v';",
            "SHOW my.custom;",
            
            "RESET nosuch;",
            "SET TRANSACTION ISOLATION LEVEL READ COMMITTED, READ ONLY;",
        ]),
    }
}

fn timeout_stmt(ctx: &mut Ctx) {
    match ctx.rng.below(5) {
        0 => ctx.push("SET statement_timeout = '5s';"),
        1 => ctx.push("SELECT pg_sleep(0.3);"),
        2 => ctx.push("SET LOCAL statement_timeout = 3000;"),
        3 => ctx.push("SELECT pg_sleep(0.01);"),
        _ => ctx.push("SET statement_timeout = 0;"),
    }
}

pub fn scenario(ctx: &mut Ctx) {
    let ncols = ctx.rng.range(2, 3) as usize;
    let ti = make_table(ctx, ncols, &TYS, true);
    let mut sp = 0;
    for _ in 0..ctx.rng.range(8, 22) {
        match ctx.rng.weighted(&[5, 8, 4, 3, 3, 3, 3, 2, 10, 2, 3, 2, 7]) {
            0 => begin(ctx),
            1 => dml_stmt(ctx, ti),
            2 => {
                let k = *ctx.rng.pick(&["COMMIT", "END"]);
                let o = chain_opt(ctx);
                ctx.push(format!("{k}{o};"));
            }
            3 => {
                let k = *ctx.rng.pick(&["ROLLBACK", "ABORT"]);
                let o = chain_opt(ctx);
                ctx.push(format!("{k}{o};"));
            }
            4 => ctx.push("SELECT 1;"),
            5 => {
                if sp == 0 || ctx.rng.chance(35) {
                    sp += 1;
                    ctx.push(format!("SAVEPOINT sp{sp};"));
                    continue;
                }
                let n = if sp == 0 { 1 } else { ctx.rng.range(1, sp as i64) };
                let s = match ctx.rng.below(4) {
                    0 => format!("ROLLBACK TO SAVEPOINT sp{n};"),
                    1 => format!("ROLLBACK TO sp{n};"),
                    2 => format!("RELEASE sp{n};"),
                    _ => format!("RELEASE SAVEPOINT sp{n};"),
                };
                ctx.push(s);
            }
            6 => {
                let t = ctx.tables[ti].clone();
                ctx.push(select_all(&t));
            }
            7 => ctx.push_pick(&[
                "SELECT 1;",
                "SELECT 1 WHERE false;",
            ]),
            8 => setting_stmt(ctx),
            9 => timeout_stmt(ctx),
            10 => {
                // トランザクションが要らない文・使えない文
                ctx.push_pick(&[
                    "CHECKPOINT;",
                    "SAVEPOINT;",
                    "RELEASE nosuchsp;",
                    "ROLLBACK TO nosuchsp;",
                    "BEGIN ISOLATION LEVEL BOGUS;",
                    "BEGIN READ ONLY, READ WRITE;",
                    "SELECT 1/0;",
                    "SELECT * FROM nosuch_tbl;",
                    
                    "BEGIN; BEGIN;",
                    "SAVEPOINT a; SAVEPOINT a; ROLLBACK TO a;",
                    "COMMIT; COMMIT;",
                    "BEGIN; SELECT 1/0; COMMIT;",
                    "BEGIN; SELECT 1/0; SELECT 1;",
                    "ROLLBACK; ROLLBACK;",
                    "BEGIN READ ONLY DEFERRABLE;",
                    "COMMIT AND CHAIN;",
                    "ROLLBACK AND CHAIN;",
                    "START TRANSACTION READ ONLY, ISOLATION LEVEL READ COMMITTED;",
                ]);
            }
            11 => {
                let s = error_stmt(ctx);
                ctx.push(s);
            }
            _ => super::txn_r3::extra_stmt(ctx),
        }
    }
    ctx.push("COMMIT;");
    ctx.push("RESET ALL;");
    ctx.push("DROP TABLE IF EXISTS fz_x, fz_y, fz_z, fz_tmp, fz_ro, fz_tt;");
    let t = ctx.tables[ti].clone();
    ctx.push(select_all(&t));
}
