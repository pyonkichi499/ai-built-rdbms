//! difffuzz: PostgreSQL（正解）と yuzhu に同じ SQL を同じ順で流し、結果を正規化して突き合わせる。
//! 使い方は README.md。

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
    verbose: bool,
}

fn usage() -> ! {
    eprintln!(
        "usage: difffuzz [--domain expr|types|query|dml|txn|all] [--seed N] [--case N | --start N --cases N]\n\
         \x20               [--pg CONNINFO] [--yuzhu CONNINFO] [--psql PATH] [--out FILE.jsonl]\n\
         \x20               [--timeout-ms N] [--no-message] [--skip-unsupported] [-v]\n\
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
        verbose: false,
    };
    let mut a = std::env::args().skip(1);
    let num = |a: &mut dyn Iterator<Item = String>| -> u64 {
        a.next().and_then(|v| v.parse().ok()).unwrap_or_else(|| usage())
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
            format!("{{\"status\":\"error\",\"sqlstate\":{},\"message\":{}}}", jstr(sqlstate), jstr(message))
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
        (Res::Err { sqlstate: s1, message: m1 }, Res::Err { sqlstate: s2, message: m2 }) => {
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
    kind: &'static str,
    pg: Res,
    yz: Res,
}

/// 1 シナリオを 1 セッションずつで流す。最初の差分で打ち切る（以降は状態が食い違うため）。
fn run_case(o: &Opts, domain: &str, case: u64) -> Result<(Vec<String>, Option<Diff>, bool), String> {
    let mut ctx = Ctx::new(o.seed, case);
    generate(domain, &mut ctx)?;
    let stmts = ctx.stmts.clone();
    let mut pg = Session::open(&o.psql, &o.pg).map_err(|e| format!("psql 起動失敗: {e}"))?;
    let mut yz = Session::open(&o.psql, &o.yuzhu).map_err(|e| format!("psql 起動失敗: {e}"))?;
    let mut diff = None;
    let mut skipped = false;
    for (i, sql) in stmts.iter().enumerate() {
        let rp = pg.exec(sql, o.timeout);
        let ry = yz.exec(sql, o.timeout);
        if o.verbose {
            eprintln!("[{i}] {sql}\n    pg:    {}\n    yuzhu: {}", jres(&rp), jres(&ry));
        }
        if o.skip_unsupported && matches!(&ry, Res::Err { sqlstate, .. } if sqlstate == "0A000") && matches!(rp, Res::Ok { .. }) {
            skipped = true;
            break;
        }
        if let Some(kind) = diff_kind(&rp, &ry, o.no_message) {
            diff = Some(Diff { step: i, kind, pg: rp, yz: ry });
            break;
        }
    }
    // 後始末（結果は見ない）。両方のサーバからテーブルを消す。
    for sql in ctx.cleanup_stmts() {
        pg.exec(&sql, o.timeout);
        yz.exec(&sql, o.timeout);
    }
    Ok((stmts, diff, skipped))
}

fn main() {
    let o = parse_args();
    let domains: Vec<&str> = if o.domain == "all" { DOMAINS.to_vec() } else { vec![o.domain.as_str()] };
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
        std::fs::OpenOptions::new().create(true).append(true).open(p).unwrap_or_else(|e| {
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
    let mut kinds: std::collections::BTreeMap<&str, u64> = Default::default();
    for case in first..first + count {
        // 領域は case 番号で決まる（--case でも同じ領域になる）
        let domain = domains[(case % domains.len() as u64) as usize];
        let (stmts, diff, skipped) = match run_case(&o, domain, case) {
            Ok(r) => r,
            Err(e) => {
                eprintln!("{e}");
                std::process::exit(2)
            }
        };
        nstmt += stmts.len() as u64;
        nskip += skipped as u64;
        if let Some(d) = diff {
            ndiff += 1;
            *kinds.entry(d.kind).or_default() += 1;
            let scenario = stmts[..=d.step].iter().map(|s| jstr(s)).collect::<Vec<_>>().join(",");
            let line = format!(
                "{{\"seed\":{},\"case\":{case},\"domain\":{},\"step\":{},\"kind\":{},\"sql\":{},\"pg\":{},\"yuzhu\":{},\"scenario\":[{scenario}]}}",
                o.seed,
                jstr(domain),
                d.step,
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
    eprintln!("seed={} cases={count} statements={nstmt} skipped_unsupported={nskip} diffs={ndiff} {kinds:?}", o.seed);
    std::process::exit(if ndiff > 0 { 1 } else { 0 });
}
