//! yuzhu-server: PostgreSQL wire-compatible server binary for yuzhu.
//!
//! Exit codes: 0 clean shutdown, 1 refused to start (or shutdown failed),
//! 2 immediate shutdown (SIGQUIT, nothing written), 3 PANIC (cluster
//! poisoned, no checkpoint).

#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::Parser;
use signal_hook::consts::{SIGINT, SIGQUIT, SIGTERM};
use signal_hook::iterator::Signals;
use yuzhu_server::config::{Cli, Config};
use yuzhu_server::{Outcome, Server, ShutdownHandle, ShutdownMode};

const EXIT_IMMEDIATE: i32 = 2;
const EXIT_PANIC: i32 = 3;

/// Receives SIGTERM (smart), SIGINT (fast) and SIGQUIT (immediate) on a
/// dedicated thread and forwards them to the shutdown coordinator.
fn spawn_signal_thread(handle: ShutdownHandle) -> std::io::Result<()> {
    let mut signals = Signals::new([SIGTERM, SIGINT, SIGQUIT])?;
    std::thread::Builder::new()
        .name("signals".into())
        .spawn(move || {
            for sig in signals.forever() {
                let mode = match sig {
                    SIGTERM => ShutdownMode::Smart,
                    SIGINT => ShutdownMode::Fast,
                    _ => ShutdownMode::Immediate,
                };
                tracing::info!(signal = sig, ?mode, "shutdown requested");
                handle.request(mode);
            }
        })?;
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let config = match Config::from_cli(&cli) {
        Ok(c) => c,
        Err(e) => {
            #[allow(clippy::print_stderr)]
            {
                eprintln!("yuzhu-server: {e}");
            }
            return ExitCode::FAILURE;
        }
    };
    tracing_subscriber::fmt()
        .with_max_level(config.log_level)
        .init();
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        data_directory = %config.data_directory.display(),
        max_connections = config.max_connections,
        shared_buffers_frames = config.shared_buffers,
        "starting yuzhu-server"
    );
    let server = match Server::bind(&config) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, listen = %config.listen, port = config.port, "cannot start");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = spawn_signal_thread(server.shutdown_handle()) {
        tracing::error!(error = %e, "cannot install signal handlers");
        return ExitCode::FAILURE;
    }
    match server.run() {
        Ok(Outcome::Stopped) => {
            tracing::info!("server stopped");
            ExitCode::SUCCESS
        }
        Ok(Outcome::Immediate) => {
            // Exit at once: no destructors, no checkpoint.
            std::process::exit(EXIT_IMMEDIATE)
        }
        Ok(Outcome::Poisoned) => std::process::exit(EXIT_PANIC),
        Err(e) => {
            tracing::error!(error = %e, "server stopped with an error");
            ExitCode::FAILURE
        }
    }
}
