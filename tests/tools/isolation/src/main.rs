//! yuzhu-isolation: `PostgreSQL` の isolationtester と同じ spec 形式を読み、
//! Simple Query プロトコルだけで permutation を実行するテストランナー。
//!
//! 使い方は `tests/tools/isolation/README.md` を参照。
#![forbid(unsafe_code)]

mod conn;
#[cfg(test)]
mod fake_server;
mod junit;
mod ledger;
mod runner;
mod spec;
mod split;

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::Parser;

use crate::conn::ConnParams;
use crate::junit::{Case, CaseResult};
use crate::ledger::Ledger;
use crate::runner::{BlockingDetection, Options, Protocol};

/// 結果の出力形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Format {
    /// 標準出力に `ok` / `FAILED` を出すだけ。
    Text,
    /// 加えて `JUnit` XML を `--format-file` に書く。
    Junit,
}

/// isolationtester 互換の spec ランナー（Simple / Extended Query）。
#[derive(Debug, Parser)]
#[command(name = "yuzhu-isolation", version)]
struct Cli {
    /// 実行する .spec ファイル、または .spec を含むディレクトリ。
    #[arg(required = true)]
    specs: Vec<PathBuf>,

    /// 接続先ホスト（`/` で始まる場合は UNIX ソケットのディレクトリ）。
    #[arg(long, default_value = "127.0.0.1")]
    host: String,
    #[arg(long, short = 'p', default_value_t = 5432)]
    port: u16,
    #[arg(long, short = 'U', default_value = "postgres")]
    user: String,
    #[arg(long, short = 'd', default_value = "postgres")]
    dbname: String,
    /// パスワード（MD5 / SCRAM-SHA-256 / 平文認証で使う）。
    #[arg(long, env = "PGPASSWORD", hide_env_values = true)]
    password: Option<String>,

    /// 起動パケットで送る実行時パラメータ（`NAME=VALUE`、複数可）。
    /// 例: `--set 'datestyle=Postgres, MDY'`（`pg_isolation_regress` は PGDATESTYLE をこの値にする）。
    #[arg(long = "set", value_name = "NAME=VALUE", value_parser = parse_kv)]
    startup_params: Vec<(String, String)>,

    /// ステップの SQL の送り方。extended は文に分けて Parse / Bind / Execute で送り、最後に Sync する。
    /// 制御接続の待ち判定は、この指定に関わらず（pg 判定なら）Extended Query を使う。
    #[arg(long, value_enum, default_value_t = Protocol::Simple)]
    protocol: Protocol,
    /// 待ち時間（`--max-step-wait` と `--block-timeout-ms`）に掛ける倍率。遅い CI 用。
    #[arg(long, env = "YUZHU_TIMEOUT_SCALE", default_value_t = 1.0, value_parser = parse_scale)]
    timeout_scale: f64,
    /// 流さない spec の一覧（`<name> <KD-n> <理由>` の行）。飛ばしたものは `SKIP <name>: KD-n` と出す。
    #[arg(long)]
    skip_list: Option<PathBuf>,
    /// 既知の差の台帳。既定は spec のディレクトリから親をたどって探す `known-diffs.toml`。
    #[arg(long)]
    known_diffs: Option<PathBuf>,
    /// `--variant` の台帳検査（variant ファイルが台帳の `affects` に載っているか）をしない。
    #[arg(long)]
    no_kd_check: bool,
    /// 出力形式。junit のときは `--format-file` に `JUnit` XML を書く。
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
    /// `--format junit` の書き出し先。
    #[arg(long, default_value = "isolation-junit.xml")]
    format_file: PathBuf,

    /// ロック待ちの判定方法。pg は `pg_isolation_test_session_is_blocked()` を使う。
    #[arg(long, value_enum, default_value_t = BlockingDetection::Pg)]
    blocking_detection: BlockingDetection,
    /// --blocking-detection=timeout のとき、この時間応答がなければ待ちとみなす（ミリ秒）。
    #[arg(long, default_value_t = 300)]
    block_timeout_ms: u64,
    /// 1 ステップの最大待ち時間（秒）。超えたら `CancelRequest` を送り、2 倍で打ち切る。
    /// isolationtester の `max_step_wait（既定` 360 秒）に相当する。
    #[arg(long, default_value_t = 30)]
    max_step_wait: u64,

    /// 期待ファイルのディレクトリ。既定は spec のディレクトリの隣の `expected/`。
    #[arg(long)]
    expected_dir: Option<PathBuf>,
    /// 実装別の期待ファイル名（`<name>.<VARIANT>.out`）。あればそれも正解の候補にする。
    #[arg(long)]
    variant: Option<String>,
    /// 実際の出力を `<DIR>/<name>.out` に書き出す。
    #[arg(long)]
    results_dir: Option<PathBuf>,
    /// 比較せず、出力をそのまま標準出力に書く。
    #[arg(long, conflicts_with = "accept")]
    print: bool,
    /// 実際の出力で期待ファイルを上書きする（本物の `PostgreSQL` に対してだけ使うこと）。
    /// --variant を付けると `<name>.<VARIANT>.out` に書く。
    #[arg(long)]
    accept: bool,
}

fn parse_kv(s: &str) -> Result<(String, String), String> {
    s.split_once('=')
        .map(|(k, v)| (k.trim().to_owned(), v.to_owned()))
        .filter(|(k, _)| !k.is_empty())
        .ok_or_else(|| format!("expected NAME=VALUE, got \"{s}\""))
}

fn collect_specs(paths: &[PathBuf]) -> io::Result<Vec<PathBuf>> {
    let mut specs = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut found: Vec<PathBuf> = fs::read_dir(p)?
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|f| f.extension().is_some_and(|x| x == "spec"))
                .collect();
            found.sort();
            specs.extend(found);
        } else {
            specs.push(p.clone());
        }
    }
    Ok(specs)
}

fn expected_dir_for(cli: &Cli, spec: &Path) -> PathBuf {
    if let Some(d) = &cli.expected_dir {
        return d.clone();
    }
    let dir = spec.parent().unwrap_or_else(|| Path::new("."));
    dir.parent().unwrap_or(dir).join("expected")
}

/// `pg_regress` と同じく `<name>.out` と `<name>_1.out` … `<name>_9.out` を候補にする。
/// `--variant` があれば `<name>.<variant>.out` を先頭に足す。
fn expected_candidates(dir: &Path, name: &str, variant: Option<&str>) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if let Some(var) = variant {
        v.push(dir.join(format!("{name}.{var}.out")));
    }
    v.push(dir.join(format!("{name}.out")));
    for i in 1..=9 {
        v.push(dir.join(format!("{name}_{i}.out")));
    }
    v.into_iter().filter(|p| p.is_file()).collect()
}

fn diff_text(expected_path: &Path, expected: &str, actual: &str) -> String {
    similar::TextDiff::from_lines(expected, actual)
        .unified_diff()
        .context_radius(3)
        .header(&expected_path.display().to_string(), "actual")
        .to_string()
}

/// 1 つの spec の結果の扱い。
fn run_one(
    cli: &Cli,
    opts: &Options,
    spec_path: &Path,
    ledger: &mut Option<Result<Ledger, String>>,
) -> io::Result<Case> {
    let name = spec_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let case = |millis: u128, result: CaseResult| Case {
        name: name.clone(),
        millis,
        result,
    };
    let src = fs::read_to_string(spec_path)?;
    let exp_dir = expected_dir_for(cli, spec_path);

    // 台帳にない variant ファイルがあれば、流す前に失敗にする。
    if !cli.accept
        && !cli.print
        && !cli.no_kd_check
        && let Some(v) = &cli.variant
        && exp_dir.join(format!("{name}.{v}.out")).is_file()
        && let Err(msg) = check_variant(cli, spec_path, ledger)
    {
        println!("FAILED   {name:<40}      0 ms  ({msg})");
        return Ok(case(0, CaseResult::Fail(msg)));
    }

    let started = Instant::now();
    let (output, fatal) = match spec::parse(&src) {
        Ok(spec) => runner::run_spec(&spec, &name, opts),
        Err(e) => (format!("{e}\n"), true),
    };
    let elapsed = started.elapsed().as_millis();

    if let Some(dir) = &cli.results_dir {
        fs::create_dir_all(dir)?;
        fs::write(dir.join(format!("{name}.out")), &output)?;
    }
    if cli.print {
        io::stdout().write_all(output.as_bytes())?;
        return Ok(case(
            elapsed,
            if fatal {
                CaseResult::Fail("aborted".into())
            } else {
                CaseResult::Pass
            },
        ));
    }
    if cli.accept {
        fs::create_dir_all(&exp_dir)?;
        let file = match &cli.variant {
            Some(v) => format!("{name}.{v}.out"),
            None => format!("{name}.out"),
        };
        let path = exp_dir.join(file);
        fs::write(&path, &output)?;
        println!("accepted {name:<40} {elapsed:>6} ms  -> {}", path.display());
        return Ok(case(
            elapsed,
            if fatal {
                CaseResult::Fail("aborted".into())
            } else {
                CaseResult::Pass
            },
        ));
    }

    let candidates = expected_candidates(&exp_dir, &name, cli.variant.as_deref());
    if candidates.is_empty() {
        let msg = format!("no expected file in {}", exp_dir.display());
        println!("FAILED   {name:<40} {elapsed:>6} ms  ({msg})");
        return Ok(case(elapsed, CaseResult::Fail(msg)));
    }
    let mut best: Option<(usize, String)> = None;
    for c in &candidates {
        let expected = fs::read_to_string(c)?;
        if expected == output {
            println!("ok       {name:<40} {elapsed:>6} ms");
            return Ok(case(elapsed, CaseResult::Pass));
        }
        let d = diff_text(c, &expected, &output);
        let size = d.lines().count();
        if best.as_ref().is_none_or(|(s, _)| size < *s) {
            best = Some((size, d));
        }
    }
    println!("FAILED   {name:<40} {elapsed:>6} ms");
    let diff = best.map(|(_, d)| d).unwrap_or_default();
    print!("{diff}");
    Ok(case(elapsed, CaseResult::Fail(diff)))
}

/// variant ファイルを使う spec が台帳の `affects` に載っているかを確かめる。
fn check_variant(
    cli: &Cli,
    spec_path: &Path,
    ledger: &mut Option<Result<Ledger, String>>,
) -> Result<(), String> {
    let loaded = ledger.get_or_insert_with(|| {
        let start = spec_path.parent().unwrap_or_else(|| Path::new("."));
        let path = ledger::find_ledger(cli.known_diffs.as_deref(), start).ok_or_else(|| {
            "known-diffs.toml not found (use --known-diffs or --no-kd-check)".to_owned()
        })?;
        Ledger::load(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))
    });
    let ledger = loaded.as_ref().map_err(Clone::clone)?;
    let key = ledger::affects_key(spec_path);
    if ledger.affects_test(&key) {
        Ok(())
    } else {
        Err(format!(
            "variant file exists but \"{key}\" is not in any known-diffs.toml affects"
        ))
    }
}

/// `d` を `factor` 倍する（桁あふれ・不正な値のときは `d` のまま）。
fn scale_duration(d: Duration, factor: f64) -> Duration {
    Duration::try_from_secs_f64(d.as_secs_f64() * factor).unwrap_or(d)
}

fn parse_scale(s: &str) -> Result<f64, String> {
    let v: f64 = s.trim().parse().map_err(|e| format!("{e}"))?;
    if v.is_finite() && v > 0.0 {
        Ok(v)
    } else {
        Err("must be a positive number".to_owned())
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let opts = Options {
        conn: ConnParams {
            host: cli.host.clone(),
            port: cli.port,
            user: cli.user.clone(),
            dbname: cli.dbname.clone(),
            password: cli.password.clone(),
            startup_params: cli.startup_params.clone(),
        },
        protocol: cli.protocol,
        detection: cli.blocking_detection,
        block_timeout: scale_duration(
            Duration::from_millis(cli.block_timeout_ms),
            cli.timeout_scale,
        ),
        max_step_wait: scale_duration(Duration::from_secs(cli.max_step_wait), cli.timeout_scale),
    };
    let specs = match collect_specs(&cli.specs) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("yuzhu-isolation: {e}");
            return ExitCode::from(2);
        }
    };
    let skips = match &cli.skip_list {
        Some(p) => match fs::read_to_string(p)
            .map_err(|e| format!("cannot read {}: {e}", p.display()))
            .and_then(|t| ledger::parse_skip_list(&t))
        {
            Ok(m) => m,
            Err(e) => {
                eprintln!("yuzhu-isolation: {e}");
                return ExitCode::from(2);
            }
        },
        None => HashMap::new(),
    };

    let mut ledger_slot: Option<Result<Ledger, String>> = None;
    let mut cases = Vec::new();
    for s in &specs {
        let name = s
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if let Some(e) = skips.get(&name) {
            println!("SKIP {name}: {} {}", e.kd, e.reason);
            cases.push(Case {
                name,
                millis: 0,
                result: CaseResult::Skip(format!("{} {}", e.kd, e.reason)),
            });
            continue;
        }
        match run_one(&cli, &opts, s, &mut ledger_slot) {
            Ok(c) => cases.push(c),
            Err(e) => {
                println!("FAILED   {}: {e}", s.display());
                cases.push(Case {
                    name,
                    millis: 0,
                    result: CaseResult::Fail(e.to_string()),
                });
            }
        }
    }
    let failed = cases
        .iter()
        .filter(|c| matches!(c.result, CaseResult::Fail(_)))
        .count();
    let skipped = cases
        .iter()
        .filter(|c| matches!(c.result, CaseResult::Skip(_)))
        .count();
    let ran = cases.len() - skipped;
    if !cli.print {
        if failed == 0 {
            println!("\nAll {ran} tests passed.");
        } else {
            println!("\n{failed} of {ran} tests failed.");
        }
        if skipped > 0 {
            println!("{skipped} tests skipped.");
        }
    }
    if cli.format == Format::Junit {
        let xml = junit::render("yuzhu-isolation", &cases);
        if let Err(e) = fs::write(&cli.format_file, xml) {
            eprintln!(
                "yuzhu-isolation: cannot write {}: {e}",
                cli.format_file.display()
            );
            return ExitCode::from(2);
        }
    }
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timeout_scale_multiplies_durations() {
        assert_eq!(
            scale_duration(Duration::from_secs(30), 2.0),
            Duration::from_secs(60)
        );
        assert_eq!(
            scale_duration(Duration::from_millis(300), 0.5),
            Duration::from_millis(150)
        );
        assert_eq!(
            scale_duration(Duration::from_secs(30), f64::NAN),
            Duration::from_secs(30)
        );
    }

    #[test]
    fn scale_validation() {
        assert_eq!(parse_scale("2.5"), Ok(2.5));
        assert!(parse_scale("0").is_err());
        assert!(parse_scale("-1").is_err());
        assert!(parse_scale("nan").is_err());
        assert!(parse_scale("x").is_err());
    }

    #[test]
    fn cli_flags() {
        let c = Cli::try_parse_from(["x", "a.spec"]).unwrap();
        assert_eq!(c.protocol, Protocol::Simple);
        assert_eq!(c.blocking_detection, BlockingDetection::Pg);
        assert!((c.timeout_scale - 1.0).abs() < f64::EPSILON);
        let c = Cli::try_parse_from([
            "x",
            "--protocol",
            "extended",
            "--timeout-scale",
            "3",
            "--format",
            "junit",
            "--no-kd-check",
            "a.spec",
        ])
        .unwrap();
        assert_eq!(c.protocol, Protocol::Extended);
        assert!((c.timeout_scale - 3.0).abs() < f64::EPSILON);
        assert_eq!(c.format, Format::Junit);
        assert!(c.no_kd_check);
        assert!(Cli::try_parse_from(["x", "--timeout-scale", "0", "a.spec"]).is_err());
    }

    fn tmpdir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("yz-iso-main-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn variant_not_in_ledger_fails() {
        let base = tmpdir("kd");
        let specs = base.join("tests/isolation/ported/specs");
        let exp = base.join("tests/isolation/ported/expected");
        fs::create_dir_all(&specs).unwrap();
        fs::create_dir_all(&exp).unwrap();
        fs::write(specs.join("foo.spec"), "session s\nstep a { SELECT 1; }\n").unwrap();
        fs::write(exp.join("foo.yuzhu-m5.out"), "x").unwrap();
        fs::write(
            base.join("tests/known-diffs.toml"),
            "[[kd]]\nid = \"KD-1\"\naffects = [\"isolation/ported/other\"]\n",
        )
        .unwrap();
        let cli = Cli::try_parse_from(["x", "--variant", "yuzhu-m5", "foo.spec"]).unwrap();
        let opts = Options {
            conn: ConnParams {
                host: "127.0.0.1".into(),
                port: 1,
                user: "u".into(),
                dbname: "d".into(),
                password: None,
                startup_params: vec![],
            },
            protocol: Protocol::Simple,
            detection: BlockingDetection::Pg,
            block_timeout: Duration::from_millis(1),
            max_step_wait: Duration::from_secs(1),
        };
        let mut slot = None;
        let c = run_one(&cli, &opts, &specs.join("foo.spec"), &mut slot).unwrap();
        assert!(matches!(&c.result, CaseResult::Fail(m) if m.contains("isolation/ported/foo")));

        // 台帳に載せれば検査を通る（接続に失敗するのでその先で失敗するが、理由は変わる）。
        fs::write(
            base.join("tests/known-diffs.toml"),
            "[[kd]]\nid = \"KD-1\"\naffects = [\"isolation/ported/foo\"]\n",
        )
        .unwrap();
        let mut slot = None;
        let c = run_one(&cli, &opts, &specs.join("foo.spec"), &mut slot).unwrap();
        assert!(!matches!(&c.result, CaseResult::Fail(m) if m.contains("known-diffs")));

        // --no-kd-check で外せる。
        let cli = Cli::try_parse_from(["x", "--variant", "yuzhu-m5", "--no-kd-check", "foo.spec"])
            .unwrap();
        fs::write(base.join("tests/known-diffs.toml"), "").unwrap();
        let mut slot = None;
        let c = run_one(&cli, &opts, &specs.join("foo.spec"), &mut slot).unwrap();
        assert!(!matches!(&c.result, CaseResult::Fail(m) if m.contains("known-diffs")));
        fs::remove_dir_all(&base).unwrap();
    }
}
