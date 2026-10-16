//! yuzhu-initdb: creates a data directory (`m2.md` 5.8).
//!
//! `yuzhu-initdb -D <dir> [-U <superuser>] [--no-sync]`

#![forbid(unsafe_code)]

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use clap::Parser;
use yuzhu_core::bootstrap::{InitAuth, InitdbOptions, initdb};
use yuzhu_core::sql::ast::Secret;
use yuzhu_core::storage::DEFAULT_RELSEG_SIZE;
use yuzhu_core::storage::vfs::LocalVfs;
use yuzhu_core::wal::DEFAULT_WAL_SEGMENT_SIZE;

/// `--auth-host` values (`md5` is the same as `scram-sha-256`).
#[derive(Clone, Copy, Debug, clap::ValueEnum)]
enum AuthArg {
    Trust,
    #[value(name = "scram-sha-256")]
    ScramSha256,
    Md5,
    Password,
}

impl From<AuthArg> for InitAuth {
    fn from(a: AuthArg) -> Self {
        match a {
            AuthArg::Trust => InitAuth::Trust,
            AuthArg::ScramSha256 | AuthArg::Md5 => InitAuth::ScramSha256,
            AuthArg::Password => InitAuth::Password,
        }
    }
}

/// First line of the password file without its line ending.
fn read_pwfile(path: &std::path::Path) -> Result<String, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("could not open password file \"{}\": {e}", path.display()))?;
    let line = text.lines().next().unwrap_or("");
    if line.is_empty() {
        return Err(format!("password file \"{}\" is empty", path.display()));
    }
    Ok(line.to_owned())
}

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
    /// WAL segment size in MB (a power of two from 2 to 1024).
    #[arg(long, value_name = "MB", default_value_t = DEFAULT_WAL_SEGMENT_SIZE >> 20, value_parser = clap::value_parser!(u32).range(2..=1024))]
    wal_segment_size: u32,
    /// Authentication method for `host` connections in `yuzhu_hba.conf`.
    #[arg(long, value_enum, default_value_t = AuthArg::Trust, value_name = "METHOD")]
    auth_host: AuthArg,
    /// Read the superuser password from the first line of FILE.
    #[arg(long, value_name = "FILE")]
    pwfile: Option<PathBuf>,
    /// Blocks per relation segment file (testing only).
    #[arg(long, hide = true, default_value_t = DEFAULT_RELSEG_SIZE)]
    rel_seg_blocks: u32,
}

#[allow(clippy::print_stderr, clippy::print_stdout)]
fn main() -> ExitCode {
    let args = Args::parse();
    if !args.wal_segment_size.is_power_of_two() {
        eprintln!("yuzhu-initdb: --wal-segment-size must be a power of 2 (2 to 1024 MB)");
        return ExitCode::FAILURE;
    }
    let superuser_password = match args.pwfile.as_deref().map(read_pwfile).transpose() {
        Ok(p) => p.map(Secret),
        Err(e) => {
            eprintln!("yuzhu-initdb: {e}");
            return ExitCode::FAILURE;
        }
    };
    let auth_host = InitAuth::from(args.auth_host);
    if auth_host == InitAuth::Trust {
        eprintln!("yuzhu-initdb: warning: enabling \"trust\" authentication for host connections");
        eprintln!(
            "You can change this by editing yuzhu_hba.conf or using the option --auth-host the next time you run yuzhu-initdb."
        );
    }
    let vfs = Arc::new(LocalVfs::new(args.data_directory.clone()));
    let opts = InitdbOptions {
        superuser: args.username,
        no_sync: args.no_sync,
        rel_seg_blocks: args.rel_seg_blocks,
        wal_segment_size: args.wal_segment_size << 20,
        auth_host,
        superuser_password,
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
