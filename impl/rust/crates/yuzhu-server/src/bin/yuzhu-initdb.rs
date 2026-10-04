//! yuzhu-initdb: creates a data directory (`m2.md` 5.8).
//!
//! `yuzhu-initdb -D <dir> [-U <superuser>] [--no-sync]`

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use yuzhu_core::bootstrap::{InitdbOptions, initdb};
use yuzhu_core::storage::DEFAULT_RELSEG_SIZE;
use yuzhu_core::storage::vfs::LocalVfs;

/// Creates a new yuzhu data directory (UTF8, C locale).
#[derive(Parser, Debug)]
#[command(name = "yuzhu-initdb", version, about)]
struct Args {
    /// Data directory to create (must not exist or be empty).
    #[arg(short = 'D', long = "pgdata", value_name = "DIR")]
    data_directory: PathBuf,
    /// Name of the bootstrap superuser.
    #[arg(short = 'U', long = "username", default_value = "postgres")]
    username: String,
    /// Do not fsync; faster, unsafe against OS crashes.
    #[arg(long)]
    no_sync: bool,
    /// Blocks per relation segment file (testing only).
    #[arg(long, hide = true, default_value_t = DEFAULT_RELSEG_SIZE)]
    rel_seg_blocks: u32,
}

#[allow(clippy::print_stderr, clippy::print_stdout)]
fn main() -> ExitCode {
    let args = Args::parse();
    let vfs = Arc::new(LocalVfs::new(args.data_directory.clone()));
    let opts = InitdbOptions {
        superuser: args.username,
        no_sync: args.no_sync,
        rel_seg_blocks: args.rel_seg_blocks,
    };
    match initdb(vfs, &opts) {
        Ok(()) => {
            println!(
                "Success. The database cluster in \"{}\" was initialized (superuser \"{}\").",
                args.data_directory.display(),
                opts.superuser
            );
            println!(
                "Start it with: yuzhu-server -D {}",
                args.data_directory.display()
            );
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("yuzhu-initdb: {}", e.message);
            if let Some(d) = &e.detail {
                eprintln!("DETAIL:  {d}");
            }
            if let Some(h) = &e.hint {
                eprintln!("HINT:  {h}");
            }
            ExitCode::FAILURE
        }
    }
}
