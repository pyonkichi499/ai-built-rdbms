//! difffuzz: PostgreSQL（正解）と yuzhu に同じ SQL を同じ順で流し、結果を正規化して突き合わせる。
//! 使い方は README.md。

mod compare;
mod gen;
mod rng;
mod session;

use gen::{generate, Ctx, DOMAINS};
use session::{Res, Session};
use std::io::Write;
use std::time::Duration;

struct Opts {
    domain: String,
    seed: u64,
    case: Option<u64>,
    start: u64,
    cases: u64,
    pg: String,
    yuzhu: String,
    psql: String,
    out: Option<String>,
    timeout: Duration,
    no_message: bool,
    skip_unsupported: bool,
    skip_unsupported_legacy: bool,
    ignore_trailing_space: bool,
    exclude: Vec<String>,
    verbose: bool,
}

fn usage() -> ! {
    eprintln!(
        "usage: difffuzz [--domain expr|types|query|dml|txn|join|agg|subquery|setop|index|ddl|all] [--seed N] [--case N | --start N --cases N]\n\
         \x20               [--pg CONNINFO] [--yuzhu CONNINFO] [--psql PATH] [--out FILE.jsonl]\n\
         \x20               [--timeout-ms N] [--no-message] [--skip-unsupported] [--skip-unsupported-legacy] [--ignore-trailing-space] [--exclude NEEDLE]... [-v]\n\
         defaults: --domain all --cases 1000 --pg 'host=127.0.0.1 port=55432 user=postgres dbname=postgres'\n\
         \x20         --yuzhu 'host=127.0.0.1 port=55433 user=postgres dbname=postgres'"
    );
    std::process::exit(2)
}

fn parse_args() -> Opts {
    let mut o = Opts {
        domain: "all".into(),
        seed: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() % 1_000_000)
            .unwrap_or(1),
        case: None,
        start: 0,
        cases: 1000,
        pg: std::env::var("DIFFFUZZ_PG")
            .unwrap_or_else(|_| "host=127.0.0.1 port=55432 user=postgres dbname=postgres".into()),
        yuzhu: std::env::var("DIFFFUZZ_YUZHU")
            .unwrap_or_else(|_| "host=127.0.0.1 port=55433 user=postgres dbname=postgres".into()),
        psql: std::env::var("PSQL").unwrap_or_else(|_| "/usr/lib/postgresql/17/bin/psql".into()),
        out: None,
        timeout: Duration::from_secs(10),
        no_message: false,
        skip_unsupported: false,
        skip_unsupported_legacy: false,
        ignore_trailing_space: false,
        exclude: Vec::new(),
        verbose: false,
    };
    let mut a = std::env::args().skip(1);
    let num = |a: &mut dyn Iterator<Item = String>| -> u64 {
        a.next()
            .and_then(|v| v.parse().ok())
            .unwrap_or_else(|| usage())
    };
    while let Some(k) = a.next() {
        match k.as_str() {
            "--domain" => o.domain = a.next().unwrap_or_else(|| usage()),
            "--seed" => o.seed = num(&mut a),
            "--case" => o.case = Some(num(&mut a)),
            "--start" => o.start = num(&mut a),
            "--cases" => o.cases = num(&mut a),
            "--pg" => o.pg = a.next().unwrap_or_else(|| usage()),
            "--yuzhu" => o.yuzhu = a.next().unwrap_or_else(|| usage()),
            "--psql" => o.psql = a.next().unwrap_or_else(|| usage()),
            "--out" => o.out = a.next(),
            "--timeout-ms" => o.timeout = Duration::from_millis(num(&mut a)),
            "--no-message" => o.no_message = true,
            "--skip-unsupported" => o.skip_unsupported = true,
            "--skip-unsupported-legacy" => o.skip_unsupported_legacy = true,
            "--ignore-trailing-space" => o.ignore_trailing_space = true,
            "--exclude" => o.exclude.push(a.next().unwrap_or_else(|| usage())),
            "-v" | "--verbose" => o.verbose = true,
            _ => usage(),
        }
    }
    o
}

fn jstr(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '"' => o.push_str("\\\""),
            '\\' => o.push_str("\\\\"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c if (c as u32) < 0x20 => o.push_str(&format!("\\u{:04x}", c as u32)),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn jres(r: &Res) -> String {
    match r {
        Res::Ok { rows, tag } => format!(
            "{{\"status\":\"ok\",\"rows\":[{}],\"tag\":{}}}",
            rows.iter().map(|r| jstr(r)).collect::<Vec<_>>().join(","),
            jstr(tag)
        ),
        Res::Err { sqlstate, message } => {
            format!(
                "{{\"status\":\"error\",\"sqlstate\":{},\"message\":{}}}",
                jstr(sqlstate),
                jstr(message)
            )
        }
        Res::Timeout => "{\"status\":\"timeout\"}".into(),
        Res::Disconnected(m) => format!("{{\"status\":\"disconnected\",\"detail\":{}}}", jstr(m)),
    }
}

/// 差分の種類。None なら一致。
fn diff_kind(pg: &Res, yz: &Res, no_message: bool) -> Option<&'static str> {
    match (pg, yz) {
        (Res::Ok { rows: r1, tag: t1 }, Res::Ok { rows: r2, tag: t2 }) => {
            if r1 != r2 {
                Some("rows")
            } else if t1 != t2 {
                Some("tag")
            } else {
                None
            }
        }
        (
            Res::Err {
                sqlstate: s1,
                message: m1,
            },
            Res::Err {
                sqlstate: s2,
                message: m2,
            },
        ) => {
            if s1 != s2 {
                Some("sqlstate")
            } else if m1 != m2 && !no_message {
                Some("message")
            } else {
                None
            }
        }
        (Res::Ok { .. }, Res::Err { .. }) => Some("yuzhu_error_pg_ok"),
        (Res::Err { .. }, Res::Ok { .. }) => Some("yuzhu_ok_pg_error"),
        _ => Some("status"),
    }
}

struct Diff {
    step: usize,
    /// 変種の検査（プラン変種・索引の有無）のとき、基準にした文の添字
    paired: Option<usize>,
    kind: &'static str,
    pg: Res,
    yz: Res,
}

/// 1 シナリオを 1 セッションずつで流す。最初の差分で打ち切る（以降は状態が食い違うため）。
struct CaseResult {
    stmts: Vec<String>,
    diff: Option<Diff>,
    skipped: bool,
    inconclusive: u64,
}

fn run_case(o: &Opts, domain: &str, case: u64) -> Result<CaseResult, String> {
    let mut ctx = Ctx::new(o.seed, case);
    generate(domain, &mut ctx)?;
    let suffix = format!("_{}_{case}", o.seed);
    let stmts: Vec<String> = ctx
        .stmts
        .iter()
        .map(|s| uniquify_shared_names(s, &suffix))
        .collect();
    let mut pg = Session::open(&o.psql, &o.pg).map_err(|e| format!("psql 起動失敗: {e}"))?;
    let mut yz = Session::open(&o.psql, &o.yuzhu).map_err(|e| format!("psql 起動失敗: {e}"))?;
    let mut diff = None;
    let mut skipped = false;
    let mut inconclusive = 0u64;
    let mut results: Vec<(Res, Res)> = Vec::new();
    for (i, sql) in stmts.iter().enumerate() {
        // 既知の差で他の差分が隠れるとき、その文だけ流さない（添字は保つ）
        if o.exclude.iter().any(|n| sql.contains(n.as_str())) {
            let none = Res::Ok {
                rows: Vec::new(),
                tag: "excluded".into(),
            };
            results.push((none.clone(), none));
            continue;
        }
        let (sp, sy) = (pg.send(sql), yz.send(sql));
        let rp = pg.finish(sql, sp, o.timeout);
        let ry = yz.finish(sql, sy, o.timeout);
        if o.verbose {
            eprintln!(
                "[{i}] {sql}\n    pg:    {}\n    yuzhu: {}",
                jres(&rp),
                jres(&ry)
            );
        }
        let skip_ok = o.skip_unsupported
            || (o.skip_unsupported_legacy && gen::LEGACY_DOMAINS.contains(&domain));
        if skip_ok
            && matches!(&ry, Res::Err { sqlstate, .. } if sqlstate == "0A000")
            && matches!(rp, Res::Ok { .. })
        {
            skipped = true;
            break;
        }
        let kind = match ctx.q.get(&i) {
            Some(&ordered) => match compare::compare_query(&rp, &ry, ordered, o.no_message) {
                compare::Verdict::Same => None,
                compare::Verdict::Diff(k) => Some(k),
                compare::Verdict::Inconclusive => {
                    inconclusive += 1;
                    None
                }
            },
            None => diff_kind(&rp, &ry, o.no_message),
        };
        if let Some(kind) = kind {
            diff = Some(Diff {
                step: i,
                paired: None,
                kind,
                pg: rp,
                yz: ry,
            });
            break;
        }
        results.push((rp, ry));
        // 変種の検査: 基準の文と同じサーバ上で結果が同じ
        if let Some(&(base, _)) = ctx.pairs.iter().find(|(_, v)| *v == i) {
            let ordered = ctx.q.get(&base).copied().unwrap_or(false);
            let (bp, by) = (&results[base].0, &results[base].1);
            let (vp, vy) = (&results[i].0, &results[i].1);
            let kind = if compare::variant_mismatch(bp, vp, ordered) {
                Some("variant_pg")
            } else if compare::variant_mismatch(by, vy, ordered) {
                Some("variant_yuzhu")
            } else {
                None
            };
            if let Some(kind) = kind {
                diff = Some(Diff {
                    step: i,
                    paired: Some(base),
                    kind,
                    pg: vp.clone(),
                    yz: vy.clone(),
                });
                break;
            }
        }
    }
    // 後始末（結果は見ない）。両方のサーバからテーブルを消す。
    for sql in ctx.cleanup_stmts() {
        pg.exec(&sql, o.timeout);
        yz.exec(&sql, o.timeout);
    }
    Ok(CaseResult {
        stmts,
        diff,
        skipped,
        inconclusive,
    })
}

fn main() {
    let o = parse_args();
    compare::set_ignore_trailing_space(o.ignore_trailing_space);
    let domains: Vec<&str> = if o.domain == "all" {
        DOMAINS.to_vec()
    } else {
        vec![o.domain.as_str()]
    };
    if o.domain != "all" && !DOMAINS.contains(&o.domain.as_str()) {
        usage();
    }
    // 接続確認
    for (name, conn) in [("PostgreSQL", &o.pg), ("yuzhu", &o.yuzhu)] {
        let mut s = match Session::open(&o.psql, conn) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("psql を起動できない: {e}");
                std::process::exit(2)
            }
        };
        if !matches!(s.exec("SELECT 1;", Duration::from_secs(10)), Res::Ok { .. }) {
            eprintln!("{name} に接続できない: {conn}");
            std::process::exit(2);
        }
    }
    let mut out: Option<std::fs::File> = o.out.as_ref().map(|p| {
        std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(p)
            .unwrap_or_else(|e| {
                eprintln!("--out を開けない: {e}");
                std::process::exit(2)
            })
    });

    let (first, count) = match o.case {
        Some(c) => (c, 1),
        None => (o.start, o.cases),
    };
    let mut ndiff = 0u64;
    let mut nstmt = 0u64;
    let mut nskip = 0u64;
    let mut ninconclusive = 0u64;
    let mut kinds: std::collections::BTreeMap<&str, u64> = Default::default();
    for case in first..first + count {
        // 領域は case 番号で決まる（--case でも同じ領域になる）
        let domain = domains[(case % domains.len() as u64) as usize];
        let CaseResult {
            stmts,
            diff,
            skipped,
            inconclusive,
        } = match run_case(&o, domain, case) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2)
            }
        };
        nstmt += stmts.len() as u64;
        nskip += skipped as u64;
        ninconclusive += inconclusive;
        if let Some(d) = diff {
            ndiff += 1;
            *kinds.entry(d.kind).or_default() += 1;
            let scenario = stmts[..=d.step]
                .iter()
                .map(|s| jstr(s))
                .collect::<Vec<_>>()
                .join(",");
            let line = format!(
                "{{\"seed\":{},\"case\":{case},\"domain\":{},\"step\":{},\"paired\":{},\"kind\":{},\"sql\":{},\"pg\":{},\"yuzhu\":{},\"scenario\":[{scenario}]}}",
                o.seed,
                jstr(domain),
                d.step,
                d.paired.map_or("null".to_string(), |p| p.to_string()),
                jstr(d.kind),
                jstr(&stmts[d.step]),
                jres(&d.pg),
                jres(&d.yz),
            );
            match out.as_mut() {
                Some(f) => {
                    let _ = writeln!(f, "{line}");
                }
                None => println!("{line}"),
            }
        }
        if (case - first + 1) % 200 == 0 {
            eprintln!("... {} cases, {ndiff} diffs", case - first + 1);
        }
    }
    eprintln!("seed={} cases={count} statements={nstmt} skipped_unsupported={nskip} inconclusive={ninconclusive} diffs={ndiff} {kinds:?}", o.seed);
    std::process::exit(if ndiff > 0 { 1 } else { 0 });
}

/// 生成器が固定名で作る共有の名前（`fz_x` など）。複数の difffuzz を同じサーバへ並列に流すと、
/// 別プロセスのトランザクションと衝突してロック待ちになるので、シードとケースの番号を付けて分ける。
const SHARED_NAMES: [&str; 8] = [
    "fz_sch2", "fz_sch", "fz_tmp", "fz_tt", "fz_ro", "fz_x", "fz_y", "fz_z",
];

fn uniquify_shared_names(sql: &str, suffix: &str) -> String {
    let mut out = String::with_capacity(sql.len() + 16);
    let mut rest = sql;
    while !rest.is_empty() {
        let hit = SHARED_NAMES.iter().find(|n| {
            rest.starts_with(**n)
                && !rest[n.len()..].starts_with(|c: char| c.is_ascii_alphanumeric() || c == '_')
        });
        if let Some(n) = hit {
            out.push_str(n);
            out.push_str(suffix);
            rest = &rest[n.len()..];
        } else {
            let ch = rest.chars().next().unwrap_or(' ');
            out.push(ch);
            rest = &rest[ch.len_utf8()..];
        }
    }
    out
}

#[cfg(test)]
mod rename_tests {
    use super::uniquify_shared_names;

    #[test]
    fn renames_whole_words_only() {
        assert_eq!(
            uniquify_shared_names(
                "CREATE TABLE fz_x(a int); SELECT * FROM fz_x2, fz_sch2.t;",
                "_1_2"
            ),
            "CREATE TABLE fz_x_1_2(a int); SELECT * FROM fz_x2, fz_sch2_1_2.t;"
        );
    }
}
