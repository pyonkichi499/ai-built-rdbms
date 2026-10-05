//! yuzhu-waldump: WAL のレコードを人が読める形で出す（`m3.md` §6.12）。
//!
//! `yuzhu-waldump -D <dir> [--start LSN] [--end LSN] [--stats]`
//!
//! 担当 J が実装する。A が置いたスタブは引数を解釈するだけで、レコードは読まない。

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::Parser;

/// WAL のレコードを 1 行ずつ出す。データディレクトリのロックは取らない。
#[derive(Parser, Debug)]
#[command(name = "yuzhu-waldump", version, about)]
struct Args {
    /// データディレクトリ。
    #[arg(short = 'D', long = "pgdata", value_name = "DIR")]
    data_directory: PathBuf,
    /// 開始 LSN（`%X/%X`）。
    #[arg(long, value_name = "LSN")]
    start: Option<String>,
    /// 終了 LSN（`%X/%X`）。
    #[arg(long, value_name = "LSN")]
    end: Option<String>,
    /// 集計だけを出す。
    #[arg(long)]
    stats: bool,
}

#[allow(clippy::print_stderr)]
fn main() -> ExitCode {
    let args = Args::parse();
    eprintln!(
        "yuzhu-waldump: 未実装（担当 J が実装）: -D {}",
        args.data_directory.display()
    );
    ExitCode::from(2)
}
