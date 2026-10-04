//! yuzhu-server: PostgreSQL wire-compatible server binary for yuzhu.

#![forbid(unsafe_code)]

use std::process::ExitCode;

use clap::Parser;
use yuzhu_server::Server;
use yuzhu_server::config::{Cli, Config};

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
        database = %config.database_name,
        max_connections = config.max_connections,
        "starting yuzhu-server"
    );
    let server = match Server::bind(&config) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(error = %e, listen = %config.listen, port = config.port, "cannot bind");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = server.run() {
        tracing::error!(error = %e, "server stopped");
        return ExitCode::FAILURE;
    }
    ExitCode::SUCCESS
}
