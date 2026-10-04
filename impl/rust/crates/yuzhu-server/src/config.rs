//! Server configuration: command-line arguments and an optional TOML file.
//!
//! Precedence: command-line argument > configuration file > built-in default.

use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};

use clap::Parser;
use serde::Deserialize;

/// Command-line arguments.
#[derive(Parser, Debug, Default, Clone)]
#[command(name = "yuzhu-server", version, about = "yuzhu RDBMS server")]
pub struct Cli {
    /// Path to a TOML configuration file.
    #[arg(long, value_name = "PATH")]
    pub config: Option<PathBuf>,
    /// Address to listen on (default 127.0.0.1).
    #[arg(long, value_name = "ADDR")]
    pub listen: Option<IpAddr>,
    /// TCP port (default 5432).
    #[arg(long)]
    pub port: Option<u16>,
    /// Maximum number of concurrent client connections (default 100).
    #[arg(long, value_name = "N")]
    pub max_connections: Option<usize>,
    /// Log level: trace, debug, info, warn, error (default info).
    #[arg(long, value_name = "LEVEL")]
    pub log_level: Option<String>,
    /// Name of the (single) database (default "postgres").
    #[arg(long, value_name = "NAME")]
    pub database_name: Option<String>,
}

/// Contents of the TOML configuration file. Every key is optional.
#[derive(Deserialize, Debug, Default, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub listen: Option<IpAddr>,
    pub port: Option<u16>,
    pub max_connections: Option<usize>,
    pub log_level: Option<String>,
    pub database_name: Option<String>,
}

/// Effective server configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    pub listen: IpAddr,
    pub port: u16,
    pub max_connections: usize,
    pub log_level: tracing::Level,
    pub database_name: String,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: IpAddr::from([127, 0, 0, 1]),
            port: 5432,
            max_connections: 100,
            log_level: tracing::Level::INFO,
            database_name: "postgres".to_string(),
        }
    }
}

/// Configuration error.
#[derive(Debug)]
pub enum ConfigError {
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    Parse {
        path: PathBuf,
        source: toml::de::Error,
    },
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, source } => {
                write!(f, "cannot read config file {}: {source}", path.display())
            }
            Self::Parse { path, source } => {
                write!(f, "invalid config file {}: {source}", path.display())
            }
            Self::Invalid(m) => f.write_str(m),
        }
    }
}

impl std::error::Error for ConfigError {}

impl FileConfig {
    /// Parses TOML text.
    pub fn parse(text: &str, path: &Path) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Reads and parses a TOML file.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&text, path)
    }
}

fn parse_level(s: &str) -> Result<tracing::Level, ConfigError> {
    s.parse()
        .map_err(|_| ConfigError::Invalid(format!("invalid log level \"{s}\"")))
}

impl Config {
    /// Merges defaults, the file and the command line (in increasing
    /// precedence) and validates the result.
    pub fn merge(file: &FileConfig, cli: &Cli) -> Result<Self, ConfigError> {
        let d = Self::default();
        let log_level = match cli.log_level.as_deref().or(file.log_level.as_deref()) {
            Some(s) => parse_level(s)?,
            None => d.log_level,
        };
        let config = Self {
            listen: cli.listen.or(file.listen).unwrap_or(d.listen),
            port: cli.port.or(file.port).unwrap_or(d.port),
            max_connections: cli
                .max_connections
                .or(file.max_connections)
                .unwrap_or(d.max_connections),
            log_level,
            database_name: cli
                .database_name
                .clone()
                .or_else(|| file.database_name.clone())
                .unwrap_or(d.database_name),
        };
        if config.max_connections == 0 {
            return Err(ConfigError::Invalid(
                "max_connections must be at least 1".into(),
            ));
        }
        if config.database_name.is_empty() {
            return Err(ConfigError::Invalid(
                "database_name must not be empty".into(),
            ));
        }
        Ok(config)
    }

    /// Builds the configuration from parsed command-line arguments, loading
    /// the configuration file if `--config` was given.
    pub fn from_cli(cli: &Cli) -> Result<Self, ConfigError> {
        let file = match &cli.config {
            Some(path) => FileConfig::load(path)?,
            None => FileConfig::default(),
        };
        Self::merge(&file, cli)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(args: &[&str]) -> Cli {
        let mut v = vec!["yuzhu-server"];
        v.extend_from_slice(args);
        Cli::try_parse_from(v).unwrap()
    }

    #[test]
    fn defaults() {
        let c = Config::merge(&FileConfig::default(), &cli(&[])).unwrap();
        assert_eq!(c, Config::default());
        assert_eq!(c.listen.to_string(), "127.0.0.1");
        assert_eq!(c.port, 5432);
        assert_eq!(c.max_connections, 100);
        assert_eq!(c.log_level, tracing::Level::INFO);
        assert_eq!(c.database_name, "postgres");
    }

    #[test]
    fn cli_arguments() {
        let c = Config::merge(
            &FileConfig::default(),
            &cli(&[
                "--listen",
                "0.0.0.0",
                "--port",
                "15432",
                "--max-connections",
                "3",
                "--log-level",
                "debug",
                "--database-name",
                "yuzhu",
            ]),
        )
        .unwrap();
        assert_eq!(c.listen.to_string(), "0.0.0.0");
        assert_eq!(c.port, 15432);
        assert_eq!(c.max_connections, 3);
        assert_eq!(c.log_level, tracing::Level::DEBUG);
        assert_eq!(c.database_name, "yuzhu");
    }

    #[test]
    fn file_values_and_cli_override() {
        let file = FileConfig::parse(
            "listen = \"::1\"\nport = 6000\nmax_connections = 5\nlog_level = \"warn\"\ndatabase_name = \"db\"\n",
            Path::new("x.toml"),
        )
        .unwrap();
        let c = Config::merge(&file, &cli(&[])).unwrap();
        assert_eq!(c.listen.to_string(), "::1");
        assert_eq!(c.port, 6000);
        assert_eq!(c.max_connections, 5);
        assert_eq!(c.log_level, tracing::Level::WARN);
        assert_eq!(c.database_name, "db");

        let c = Config::merge(&file, &cli(&["--port", "7000", "--log-level", "error"])).unwrap();
        assert_eq!(c.port, 7000);
        assert_eq!(c.log_level, tracing::Level::ERROR);
        assert_eq!(c.max_connections, 5);
    }

    #[test]
    fn partial_file() {
        let file = FileConfig::parse("port = 1234\n", Path::new("x.toml")).unwrap();
        let c = Config::merge(&file, &cli(&[])).unwrap();
        assert_eq!(c.port, 1234);
        assert_eq!(c.max_connections, 100);
    }

    #[test]
    fn invalid_values() {
        assert!(FileConfig::parse("bogus = 1\n", Path::new("x.toml")).is_err());
        assert!(FileConfig::parse("port = \"abc\"\n", Path::new("x.toml")).is_err());
        assert!(Config::merge(&FileConfig::default(), &cli(&["--log-level", "loud"])).is_err());
        assert!(Config::merge(&FileConfig::default(), &cli(&["--max-connections", "0"])).is_err());
        assert!(Cli::try_parse_from(["yuzhu-server", "--port", "99999"]).is_err());
        assert!(Cli::try_parse_from(["yuzhu-server", "--listen", "nope"]).is_err());
    }

    #[test]
    fn load_missing_file() {
        let err = FileConfig::load(Path::new("/nonexistent/yuzhu.toml")).unwrap_err();
        assert!(matches!(err, ConfigError::Read { .. }));
    }
}
