//! yuzhu-isolation: `PostgreSQL` の isolationtester と同じ spec 形式を読み、
//! Simple Query プロトコルだけで permutation を実行するテストランナー。
//!
//! 使い方は `tests/tools/isolation/README.md` を参照。
#![forbid(unsafe_code)]

mod conn;
mod runner;
mod spec;

use std::fs;
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use clap::Parser;

use crate::conn::ConnParams;
use crate::runner::{BlockingDetection, Options};

/// isolationtester 互換の spec ランナー（Simple Query のみ）。
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

fn run_one(cli: &Cli, opts: &Options, spec_path: &Path) -> io::Result<bool> {
    let name = spec_path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let src = fs::read_to_string(spec_path)?;
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
        return Ok(!fatal);
    }
    let exp_dir = expected_dir_for(cli, spec_path);
    if cli.accept {
        fs::create_dir_all(&exp_dir)?;
        let file = match &cli.variant {
            Some(v) => format!("{name}.{v}.out"),
            None => format!("{name}.out"),
        };
        let path = exp_dir.join(file);
        fs::write(&path, &output)?;
        println!("accepted {name:<40} {elapsed:>6} ms  -> {}", path.display());
        return Ok(!fatal);
    }

    let candidates = expected_candidates(&exp_dir, &name, cli.variant.as_deref());
    if candidates.is_empty() {
        println!(
            "FAILED   {name:<40} {elapsed:>6} ms  (no expected file in {})",
            exp_dir.display()
        );
        return Ok(false);
    }
    let mut best: Option<(usize, String)> = None;
    for c in &candidates {
        let expected = fs::read_to_string(c)?;
        if expected == output {
            println!("ok       {name:<40} {elapsed:>6} ms");
            return Ok(true);
        }
        let d = diff_text(c, &expected, &output);
        let size = d.lines().count();
        if best.as_ref().is_none_or(|(s, _)| size < *s) {
            best = Some((size, d));
        }
    }
    println!("FAILED   {name:<40} {elapsed:>6} ms");
    if let Some((_, d)) = best {
        print!("{d}");
    }
    Ok(false)
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
        detection: cli.blocking_detection,
        block_timeout: Duration::from_millis(cli.block_timeout_ms),
        max_step_wait: Duration::from_secs(cli.max_step_wait),
    };
    let specs = match collect_specs(&cli.specs) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("yuzhu-isolation: {e}");
            return ExitCode::from(2);
        }
    };
    let mut failed = 0usize;
    for s in &specs {
        match run_one(&cli, &opts, s) {
            Ok(true) => {}
            Ok(false) => failed += 1,
            Err(e) => {
                println!("FAILED   {}: {e}", s.display());
                failed += 1;
            }
        }
    }
    if !cli.print {
        if failed == 0 {
            println!("\nAll {} tests passed.", specs.len());
        } else {
            println!("\n{failed} of {} tests failed.", specs.len());
        }
    }
    if failed == 0 {
        ExitCode::SUCCESS
    } else {
        ExitCode::FAILURE
    }
}
