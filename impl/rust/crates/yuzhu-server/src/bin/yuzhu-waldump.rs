//! yuzhu-waldump: WAL のレコードを人が読める形で出す（`m3.md` §6.12）。
//!
//! `yuzhu-waldump -D <dir> [--start LSN] [--end LSN] [--stats]`
//!
//! データディレクトリのロックは取らず、制御ファイルと `pg_wal` を読むだけ。
//! `--start` はレコードの境界を指すこと（セグメントの先頭は常に有効）。

#![forbid(unsafe_code)]

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use yuzhu_core::DebugKnobs;
use yuzhu_core::control::ControlFileHandle;
use yuzhu_core::storage::vfs::{LocalVfs, Vfs};
use yuzhu_core::wal::dump::{DumpStats, format_record_with_prev};
use yuzhu_core::wal::segment::{list_segments, seg_start};
use yuzhu_core::wal::{Lsn, WalConfig, WalReader};

/// WAL のレコードを 1 行ずつ出す。データディレクトリのロックは取らない。
#[derive(Parser, Debug)]
#[command(name = "yuzhu-waldump", version, about)]
struct Args {
    /// データディレクトリ。
    #[arg(short = 'D', long = "pgdata", value_name = "DIR")]
    data_directory: PathBuf,
    /// 開始 LSN（`%X/%X`）。省略すると最も古いセグメントの先頭。
    #[arg(long, value_name = "LSN")]
    start: Option<String>,
    /// 終了 LSN（`%X/%X`）。この LSN より前に始まるレコードだけを出す。
    #[arg(long, value_name = "LSN")]
    end: Option<String>,
    /// 集計だけを出す。
    #[arg(long)]
    stats: bool,
}

/// `%X/%X`（上位 32 ビット / 下位 32 ビット、16 進）を読む。
fn parse_lsn(s: &str) -> Result<Lsn, String> {
    let bad = || format!("invalid LSN \"{s}\" (expected %X/%X)");
    let (hi, lo) = s.split_once('/').ok_or_else(bad)?;
    let hi = u32::from_str_radix(hi, 16).map_err(|_| bad())?;
    let lo = u32::from_str_radix(lo, 16).map_err(|_| bad())?;
    Ok(Lsn((u64::from(hi) << 32) | u64::from(lo)))
}

fn lsn_str(l: Lsn) -> String {
    format!("{:X}/{:08X}", l.0 >> 32, l.0 & 0xFFFF_FFFF)
}

/// WAL を読んで `out` に書く。
fn dump(args: &Args, out: &mut dyn Write) -> Result<(), String> {
    let start_arg = args.start.as_deref().map(parse_lsn).transpose()?;
    let end_arg = args.end.as_deref().map(parse_lsn).transpose()?;
    let vfs: Arc<dyn Vfs> = Arc::new(LocalVfs::new(args.data_directory.clone()));
    let control = ControlFileHandle::open(&vfs).map_err(|e| e.message)?;
    let data = control.get();
    let cfg = WalConfig {
        segment_size: data.wal_segment_size,
        system_identifier: data.system_identifier,
        full_page_writes: true,
        knobs: DebugKnobs::default(),
    };
    let start = if let Some(l) = start_arg {
        l
    } else {
        let first = list_segments(vfs.as_ref())
            .map_err(|e| e.message)?
            .first()
            .copied()
            .ok_or_else(|| "no WAL segments found".to_string())?;
        seg_start(first, cfg.segment_size)
    };
    let mut reader = WalReader::open(Arc::clone(&vfs), &cfg, start);
    reader.set_include_switch(true);
    let mut stats = DumpStats::default();
    let mut prev = None;
    let io = |e: std::io::Error| e.to_string();
    let mut stopped_at_end = None;
    while let Some(rec) = reader.next().map_err(|e| e.message)? {
        if end_arg.is_some_and(|end| rec.start >= end) {
            stopped_at_end = end_arg;
            break;
        }
        if args.stats {
            stats.add(&rec);
        } else {
            writeln!(out, "{}", format_record_with_prev(&rec, prev)).map_err(io)?;
        }
        prev = Some(rec.start);
    }
    if args.stats {
        write!(out, "{}", stats.format()).map_err(io)?;
    }
    if let Some(end) = stopped_at_end {
        writeln!(out, "stopped at --end {}", lsn_str(end)).map_err(io)?;
    } else if let Some((lsn, reason)) = reader.end_of_wal() {
        writeln!(out, "end of WAL at {}: {reason:?}", lsn_str(lsn)).map_err(io)?;
    }
    out.flush().map_err(io)
}

#[allow(clippy::print_stderr)]
fn main() -> ExitCode {
    let args = Args::parse();
    let stdout = std::io::stdout();
    let mut out = std::io::BufWriter::new(stdout.lock());
    match dump(&args, &mut out) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            let _ = out.flush();
            eprintln!("yuzhu-waldump: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsn_parses_and_prints() {
        assert_eq!(parse_lsn("0/1000120").unwrap(), Lsn(0x0100_0120));
        assert_eq!(parse_lsn("A/FF").unwrap(), Lsn(0xA_0000_00FF));
        assert_eq!(lsn_str(Lsn(0x0100_0120)), "0/01000120");
        assert_eq!(
            parse_lsn(&lsn_str(Lsn(0x1_0000_0028))).unwrap(),
            Lsn(0x1_0000_0028)
        );
    }

    #[test]
    fn bad_lsn_is_rejected() {
        for s in ["", "0", "x/1", "1/", "/1", "1/2/3", "100000000/1"] {
            assert!(parse_lsn(s).is_err(), "{s}");
        }
    }
}
