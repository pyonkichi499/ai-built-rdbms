//! difftest: differential tester for yuzhu.
//!
//! Generates random schemas, rows and SELECT queries, runs them on a
//! reference PostgreSQL and on the server under test via the simple query
//! protocol, and compares results and SQLSTATEs. It also checks the TLP
//! (ternary logic partitioning) identity. Failing cases are minimized and
//! printed as reproducible SQL scripts.

#![forbid(unsafe_code)]

mod ast;
mod case;
mod db;
mod generate;
mod rng;
mod schema;

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::case::{Action, Case, FailKind, Failure, Pair};
use crate::db::{CmpOpts, ErrorMatch, Outcome, Server, compare};
use crate::generate::{Features, Gen, GenConfig, gen_inserts, gen_schema};
use crate::rng::Rng;
use crate::schema::{Insert, Schema};

#[derive(Parser, Debug)]
#[command(
    name = "difftest",
    version,
    about = "Differential tester: yuzhu vs PostgreSQL"
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Generate random cases and compare the two servers.
    Run(RunArgs),
    /// Run a SQL script (one statement per line, ending with `;`) on both servers and compare.
    Replay(ReplayArgs),
    /// Print the SQL a seed generates (no server needed).
    Gen(GenArgs),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
enum Level {
    /// M1: single-table SELECT, expressions, constraints.
    M1,
    /// M4: adds joins, aggregates and subqueries.
    M4,
    /// M5: adds decimal (numeric) literals.
    M5,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, ValueEnum)]
enum Feature {
    Joins,
    Aggregates,
    Subqueries,
    DecimalLiterals,
    Tlp,
}

#[derive(Args, Debug, Clone)]
struct GenOpts {
    /// Feature level (milestone).
    #[arg(long, value_enum, default_value = "m1")]
    level: Level,
    /// Enable extra features on top of the level (comma separated).
    #[arg(long, value_enum, value_delimiter = ',')]
    enable: Vec<Feature>,
    /// Disable features (comma separated), e.g. `--disable tlp`.
    #[arg(long, value_enum, value_delimiter = ',')]
    disable: Vec<Feature>,
    /// Maximum number of tables per round.
    #[arg(long, default_value_t = 2)]
    max_tables: usize,
    /// Maximum number of columns per table.
    #[arg(long, default_value_t = 5)]
    max_cols: usize,
    /// Maximum number of rows inserted per table.
    #[arg(long, default_value_t = 20)]
    max_rows: usize,
    /// Maximum expression depth.
    #[arg(long, default_value_t = 3)]
    max_depth: u32,
    /// Queries generated per round (each round has a fresh schema).
    #[arg(long, default_value_t = 50)]
    queries_per_round: u64,
    /// Percentage of queries that additionally get a TLP check.
    #[arg(long, default_value_t = 30)]
    tlp_percent: usize,
}

impl GenOpts {
    fn config(&self) -> GenConfig {
        let mut f = Features {
            tlp: true,
            ..Features::default()
        };
        if matches!(self.level, Level::M4 | Level::M5) {
            f.joins = true;
            f.aggregates = true;
            f.subqueries = true;
        }
        if self.level == Level::M5 {
            f.decimal_literals = true;
        }
        let set = |f: &mut Features, x: Feature, on: bool| match x {
            Feature::Joins => f.joins = on,
            Feature::Aggregates => f.aggregates = on,
            Feature::Subqueries => f.subqueries = on,
            Feature::DecimalLiterals => f.decimal_literals = on,
            Feature::Tlp => f.tlp = on,
        };
        for x in &self.enable {
            set(&mut f, *x, true);
        }
        for x in &self.disable {
            set(&mut f, *x, false);
        }
        GenConfig {
            features: f,
            max_tables: self.max_tables.max(1),
            max_cols: self.max_cols.max(1),
            max_rows: self.max_rows,
            max_depth: self.max_depth,
        }
    }

    fn describe(&self) -> String {
        let mut s = format!("--level {}", format!("{:?}", self.level).to_lowercase());
        let list = |v: &[Feature]| {
            v.iter()
                .map(|f| f.to_possible_value().expect("value").get_name().to_string())
                .collect::<Vec<_>>()
                .join(",")
        };
        if !self.enable.is_empty() {
            let _ = write!(s, " --enable {}", list(&self.enable));
        }
        if !self.disable.is_empty() {
            let _ = write!(s, " --disable {}", list(&self.disable));
        }
        let _ = write!(
            s,
            " --max-tables {} --max-cols {} --max-rows {} --max-depth {} --queries-per-round {} --tlp-percent {}",
            self.max_tables,
            self.max_cols,
            self.max_rows,
            self.max_depth,
            self.queries_per_round,
            self.tlp_percent
        );
        s
    }
}

#[derive(Args, Debug)]
struct ConnOpts {
    /// Reference server (PostgreSQL), libpq-style or URL connection string.
    #[arg(long = "ref", env = "DIFFTEST_REF")]
    reference: String,
    /// Server under test (yuzhu).
    #[arg(long, env = "DIFFTEST_TEST")]
    test: String,
    /// How SQLSTATEs of errors are compared.
    #[arg(long, value_enum, default_value = "exact")]
    error_match: ErrorMatch,
    /// Also compare output column names.
    #[arg(long)]
    compare_names: bool,
}

impl ConnOpts {
    fn pair(&self) -> Result<Pair, String> {
        Ok(Pair {
            r: Server::connect("reference", &self.reference)?,
            t: Server::connect("test", &self.test)?,
            cmp: CmpOpts {
                errors: self.error_match,
                names: self.compare_names,
            },
            executed: 0,
        })
    }
}

#[derive(Args, Debug)]
struct RunArgs {
    #[command(flatten)]
    conn: ConnOpts,
    #[command(flatten)]
    gopts: GenOpts,
    /// Seed (default: derived from the clock; always printed).
    #[arg(long)]
    seed: Option<u64>,
    /// Stop after this many SELECT statements (counting TLP partition queries).
    #[arg(long, default_value_t = 10_000)]
    queries: u64,
    /// First round number (to reproduce a round, use it with `--rounds 1`).
    #[arg(long, default_value_t = 0)]
    start_round: u64,
    /// Number of rounds to run (default: until `--queries` is reached).
    #[arg(long)]
    rounds: Option<u64>,
    /// Stop after this many failures.
    #[arg(long, default_value_t = 5)]
    max_failures: usize,
    /// Do not minimize failing cases.
    #[arg(long)]
    no_minimize: bool,
    /// Maximum number of re-executions per minimization.
    #[arg(long, default_value_t = 2000)]
    minimize_budget: usize,
    /// Directory where failing scripts are written.
    #[arg(long)]
    out: Option<PathBuf>,
    /// Prefix of the generated table names.
    #[arg(long, default_value = "dt")]
    prefix: String,
    /// Print every statement and both outcomes.
    #[arg(long, short)]
    verbose: bool,
}

#[derive(Args, Debug)]
struct ReplayArgs {
    #[command(flatten)]
    conn: ConnOpts,
    /// Script to run.
    file: PathBuf,
    /// Print every outcome, not only differences.
    #[arg(long, short)]
    verbose: bool,
}

#[derive(Args, Debug)]
struct GenArgs {
    #[command(flatten)]
    gopts: GenOpts,
    #[arg(long, default_value_t = 0)]
    seed: u64,
    #[arg(long, default_value_t = 0)]
    round: u64,
}

const MAX_SHOWN_ROWS: usize = 12;

fn main() {
    let cli = Cli::parse();
    let code = match cli.cmd {
        Cmd::Run(a) => run(&a),
        Cmd::Replay(a) => replay(&a),
        Cmd::Gen(a) => {
            gen_only(&a);
            Ok(0)
        }
    };
    match code {
        Ok(c) => std::process::exit(c),
        Err(e) => {
            println!("difftest: {e}");
            std::process::exit(2);
        }
    }
}

/// Everything one round generates (deterministic in `(seed, round)`).
struct Round {
    schema: Schema,
    inserts: Vec<Insert>,
    queries: Vec<(crate::ast::Select, Option<crate::ast::Expr>)>,
}

fn gen_round(seed: u64, round: u64, opts: &GenOpts, cfg: &GenConfig) -> Round {
    let mut rng = Rng::derive(seed, round);
    let schema = gen_schema(&mut rng, cfg);
    let inserts = gen_inserts(&mut rng, cfg, &schema);
    let mut queries = Vec::new();
    for _ in 0..opts.queries_per_round {
        let mut g = Gen {
            rng: &mut rng,
            cfg,
            schema: &schema,
        };
        let q = g.select();
        let tlp = (cfg.features.tlp && !q.grouped && g.rng.below(100) < opts.tlp_percent)
            .then(|| g.tlp_predicate(&q));
        queries.push((q, tlp));
    }
    Round {
        schema,
        inserts,
        queries,
    }
}

fn gen_only(a: &GenArgs) {
    let cfg = a.gopts.config();
    let r = gen_round(a.seed, a.round, &a.gopts, &cfg);
    let names: Vec<String> = (0..r.schema.tables.len())
        .map(|i| format!("t{i}"))
        .collect();
    println!(
        "-- difftest gen --seed {} --round {} {}",
        a.seed,
        a.round,
        a.gopts.describe()
    );
    let case = Case {
        schema: r.schema,
        inserts: r.inserts,
        action: Action::None,
    };
    for s in case.setup_sql(&names) {
        println!("{s};");
    }
    for (q, tlp) in &r.queries {
        println!("{};", q.render(&names));
        if let Some(p) = tlp {
            println!("-- TLP:");
            for tq in case::tlp_queries(q, p) {
                println!("{};", tq.render(&names));
            }
        }
    }
    println!("DROP TABLE IF EXISTS {};", names.join(", "));
}

struct Runner<'a> {
    args: &'a RunArgs,
    cfg: GenConfig,
    seed: u64,
    run_id: String,
    pair: Pair,
    attempt: u64,
    failures: usize,
    selects: u64,
    errors: u64,
    tlp_checks: u64,
}

impl Runner<'_> {
    fn names(&self, tag: &str, n: usize) -> Vec<String> {
        (0..n)
            .map(|i| format!("{}{}_{tag}_{i}", self.args.prefix, self.run_id))
            .collect()
    }

    fn header(&self, round: u64) -> String {
        format!(
            "difftest seed={} round={round}\nreproduce: difftest run --ref ... --test ... --seed {} --start-round {round} --rounds 1 {}",
            self.seed,
            self.seed,
            self.args.gopts.describe()
        )
    }

    fn ref_lost(f: &Failure) -> Option<String> {
        match &f.ref_out {
            Outcome::Lost(m) => Some(format!("lost connection to the reference server: {m}")),
            _ => None,
        }
    }

    /// Minimizes and reports a failure.
    fn report(
        &mut self,
        round: u64,
        case: Case,
        failure: Box<Failure>,
        names: &[String],
    ) -> Result<(), String> {
        if let Some(e) = Self::ref_lost(&failure) {
            return Err(e);
        }
        self.failures += 1;
        let (case, mut failure, names) = if self.args.no_minimize {
            (case, failure, names.to_vec())
        } else {
            self.minimize(case, failure)
        };
        failure.canonicalize(&names);
        let script = case.script(&self.header(round), &failure, MAX_SHOWN_ROWS);
        println!(
            "\n=== FAILURE #{} ({}) round {round} ===",
            self.failures,
            failure.kind.label()
        );
        print!("{script}");
        if let Some(dir) = &self.args.out {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
            let path = dir.join(format!("fail-{}-{round}-{}.sql", self.seed, self.failures));
            std::fs::write(&path, &script)
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            println!("-- written to {}", path.display());
        }
        Ok(())
    }

    fn minimize(
        &mut self,
        mut case: Case,
        mut failure: Box<Failure>,
    ) -> (Case, Box<Failure>, Vec<String>) {
        let kind = failure.kind;
        let mut names = Vec::new();
        let mut budget = self.args.minimize_budget;
        let started = Instant::now();
        // Confirm that the case reproduces on fresh tables.
        self.attempt += 1;
        let first = self.names(&format!("m{}", self.attempt), case.schema.tables.len());
        match case.run(&mut self.pair, &first) {
            Some(f) if f.kind == kind => {
                failure = f;
                names = first;
            }
            _ => {
                println!(
                    "-- note: the failure did not reproduce in isolation; reporting it unminimized"
                );
                return (case, failure, names);
            }
        }
        let mut progress = true;
        while progress && budget > 0 {
            progress = false;
            for cand in case.candidates() {
                if budget == 0 {
                    break;
                }
                budget -= 1;
                self.attempt += 1;
                let n = self.names(&format!("m{}", self.attempt), cand.schema.tables.len());
                if let Some(f) = cand.run(&mut self.pair, &n) {
                    if f.kind == kind && Self::ref_lost(&f).is_none() {
                        case = cand;
                        failure = f;
                        names = n;
                        progress = true;
                        break;
                    }
                }
            }
        }
        println!(
            "-- minimized with {} re-executions in {:.1}s",
            self.args.minimize_budget - budget,
            started.elapsed().as_secs_f64()
        );
        (case, failure, names)
    }

    fn log(&self, sql: &str, r: &Outcome, t: &Outcome) {
        if self.args.verbose {
            println!("{sql};\n  ref : {}\n  test: {}", r.summary(5), t.summary(5));
        }
    }

    /// Returns false when the run should stop.
    fn round(&mut self, round: u64) -> Result<bool, String> {
        let rd = gen_round(self.seed, round, &self.args.gopts, &self.cfg);
        let names = self.names(&round.to_string(), rd.schema.tables.len());
        self.pair.drop_tables(&names);
        let base = Case {
            schema: rd.schema.clone(),
            inserts: rd.inserts.clone(),
            action: Action::None,
        };
        for sql in base.setup_sql(&names) {
            match self.pair.check(FailKind::SetupDiff, &sql, false) {
                Ok((r, t)) => self.log(&sql, &r, &t),
                Err(f) => {
                    self.pair.drop_tables(&names);
                    self.report(round, base, f, &names)?;
                    return Ok(self.failures < self.args.max_failures);
                }
            }
        }
        for (q, tlp) in &rd.queries {
            if self.selects >= self.args.queries {
                break;
            }
            let sql = q.render(&names);
            self.selects += 1;
            let res = self.pair.check(FailKind::QueryDiff, &sql, q.ordered());
            let failure = match res {
                Ok((r, t)) => {
                    self.log(&sql, &r, &t);
                    if r.is_error() {
                        self.errors += 1;
                    }
                    None
                }
                Err(f) => Some((f, Action::Query(q.clone()))),
            };
            let failure = failure.or_else(|| {
                let p = tlp.as_ref()?;
                self.selects += 4;
                self.tlp_checks += 1;
                let f = case::run_tlp(&mut self.pair, q, p, &names)?;
                let action = if f.kind == FailKind::QueryDiff {
                    // Report the single differing partition query.
                    let qs = case::tlp_queries(q, p);
                    let which = qs
                        .iter()
                        .position(|x| x.render(&names) == f.sql)
                        .unwrap_or(0);
                    Action::Query(qs[which].clone())
                } else {
                    Action::Tlp {
                        base: q.clone(),
                        pred: p.clone(),
                    }
                };
                Some((f, action))
            });
            if let Some((f, action)) = failure {
                let lost = matches!(f.test_out, Outcome::Lost(_));
                let case = Case {
                    schema: rd.schema.clone(),
                    inserts: rd.inserts.clone(),
                    action,
                };
                self.pair.drop_tables(&names);
                self.report(round, case, f, &names)?;
                if self.failures >= self.args.max_failures {
                    return Ok(false);
                }
                if lost {
                    self.pair.t.ensure()?;
                    return Ok(true);
                }
                // The round's tables were dropped before minimizing; re-create
                // them so that the remaining queries see the same data.
                self.pair.drop_tables(&names);
                for sql in base.setup_sql(&names) {
                    let _ = self.pair.both(&sql);
                }
            }
        }
        self.pair.drop_tables(&names);
        Ok(self.selects < self.args.queries)
    }
}

fn run(a: &RunArgs) -> Result<i32, String> {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let seed = a
        .seed
        .unwrap_or(u64::try_from(now % u128::from(u64::MAX)).expect("fits"));
    let run_id = format!(
        "{:x}",
        Rng::new(seed ^ u64::try_from(now & 0xFFFF_FFFF).expect("fits")).next_u64() & 0xFF_FFFF
    );
    let mut r = Runner {
        args: a,
        cfg: a.gopts.config(),
        seed,
        run_id,
        pair: a.conn.pair()?,
        attempt: 0,
        failures: 0,
        selects: 0,
        errors: 0,
        tlp_checks: 0,
    };
    println!("difftest: seed={seed} {}", a.gopts.describe());
    let started = Instant::now();
    let mut round = a.start_round;
    let mut rounds = 0;
    loop {
        if a.rounds.is_some_and(|n| rounds >= n) {
            break;
        }
        rounds += 1;
        if !r.round(round)? {
            break;
        }
        round += 1;
    }
    println!(
        "difftest: {} rounds, {} SELECTs ({} errored identically on both), {} TLP checks, {} statements in total, {} failure(s), {:.1}s",
        rounds,
        r.selects,
        r.errors,
        r.tlp_checks,
        r.pair.executed,
        r.failures,
        started.elapsed().as_secs_f64()
    );
    Ok(i32::from(r.failures > 0))
}

/// Statement is ordered if it has an ORDER BY at parenthesis depth 0.
fn top_level_order_by(sql: &str) -> bool {
    let mut depth = 0i32;
    let mut in_str = false;
    let bytes = sql.as_bytes();
    let upper = sql.to_ascii_uppercase();
    for (i, &b) in bytes.iter().enumerate() {
        match b {
            b'\'' => in_str = !in_str,
            b'(' if !in_str => depth += 1,
            b')' if !in_str => depth -= 1,
            b'O' | b'o' if !in_str && depth == 0 && upper[i..].starts_with("ORDER BY") => {
                return true;
            }
            _ => {}
        }
    }
    false
}

fn replay(a: &ReplayArgs) -> Result<i32, String> {
    let text = std::fs::read_to_string(&a.file)
        .map_err(|e| format!("cannot read {}: {e}", a.file.display()))?;
    let mut pair = a.conn.pair()?;
    let mut stmts = Vec::new();
    let mut cur = String::new();
    for line in text.lines() {
        let l = line.trim();
        if l.is_empty() || l.starts_with("--") {
            continue;
        }
        if !cur.is_empty() {
            cur.push('\n');
        }
        cur.push_str(l);
        if l.ends_with(';') {
            cur.pop();
            stmts.push(std::mem::take(&mut cur));
        }
    }
    if !cur.trim().is_empty() {
        stmts.push(cur);
    }
    let mut diffs = 0;
    for sql in &stmts {
        let (r, t) = pair.both(sql);
        let diff = compare(&r, &t, top_level_order_by(sql), pair.cmp);
        if diff.is_some() || a.verbose {
            println!("{sql};");
            if let Some(d) = &diff {
                println!("  DIFF: {d}");
                diffs += 1;
            }
            println!("  reference: {}", r.summary(MAX_SHOWN_ROWS));
            println!("  test     : {}", t.summary(MAX_SHOWN_ROWS));
        }
    }
    println!(
        "difftest replay: {} statement(s), {diffs} difference(s)",
        stmts.len()
    );
    Ok(i32::from(diffs > 0))
}
