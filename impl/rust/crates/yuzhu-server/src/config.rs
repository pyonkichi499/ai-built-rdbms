//! Server configuration: command-line arguments and an optional TOML file.
//!
//! Precedence: command-line argument > configuration file > built-in default.
//! The data directory is resolved as `-D` > `data_directory` in the file >
//! the `YUZHU_DATA` environment variable.

use std::fmt;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::time::Duration;

use clap::Parser;
use serde::Deserialize;

/// Size of one buffer frame (a page), the unit of a bare `shared_buffers`.
const BLOCK_BYTES: u64 = 8192;
/// PostgreSQL's minimum for `shared_buffers` (128kB).
const MIN_FRAMES: usize = 16;

/// Command-line arguments.
#[derive(Parser, Debug, Default, Clone)]
#[command(name = "yuzhu-server", version, about = "yuzhu RDBMS server")]
pub struct Cli {
    /// Data directory (created by `yuzhu-initdb`).
    #[arg(short = 'D', long = "data-directory", value_name = "DIR")]
    pub data_directory: Option<PathBuf>,
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
    /// Buffer pool size, e.g. `128MB` or `16384` (8kB blocks). Default 128MB.
    #[arg(long, value_name = "SIZE")]
    pub shared_buffers: Option<String>,
    /// Seconds between periodic checkpoints (default 300).
    #[arg(long, value_name = "SECONDS")]
    pub checkpoint_timeout: Option<u64>,
    /// Start even if the last shutdown was not clean (M2 has no recovery).
    #[arg(long)]
    pub ignore_unclean_shutdown: bool,
    /// Log level: trace, debug, info, warn, error (default info).
    #[arg(long, value_name = "LEVEL")]
    pub log_level: Option<String>,
}

/// A size given either as a number or as a string such as `"128MB"`.
#[derive(Deserialize, Debug, Clone, PartialEq, Eq)]
#[serde(untagged)]
pub enum SizeValue {
    Number(u64),
    Text(String),
}

/// Contents of the TOML configuration file. Every key is optional.
#[derive(Deserialize, Debug, Default, Clone, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct FileConfig {
    pub data_directory: Option<PathBuf>,
    pub listen: Option<IpAddr>,
    pub port: Option<u16>,
    pub max_connections: Option<usize>,
    pub shared_buffers: Option<SizeValue>,
    pub checkpoint_timeout: Option<u64>,
    pub ignore_unclean_shutdown: Option<bool>,
    pub log_level: Option<String>,
}

/// Effective server configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// Empty only for servers started on an existing `Cluster` (tests).
    pub data_directory: PathBuf,
    pub listen: IpAddr,
    pub port: u16,
    pub max_connections: usize,
    /// Number of 8kB buffer frames.
    pub shared_buffers: usize,
    pub checkpoint_timeout: Duration,
    pub ignore_unclean_shutdown: bool,
    pub log_level: tracing::Level,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            data_directory: PathBuf::new(),
            listen: IpAddr::from([127, 0, 0, 1]),
            port: 5432,
            max_connections: 100,
            shared_buffers: 16384,
            checkpoint_timeout: Duration::from_secs(300),
            ignore_unclean_shutdown: false,
            log_level: tracing::Level::INFO,
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

/// Parses a `shared_buffers` value into a number of frames. A bare number
/// counts 8kB blocks (like PostgreSQL); `kB`, `MB` and `GB` suffixes are
/// accepted.
pub fn parse_shared_buffers(value: &SizeValue) -> Result<usize, ConfigError> {
    let shown = match value {
        SizeValue::Number(n) => n.to_string(),
        SizeValue::Text(s) => s.clone(),
    };
    let invalid = || ConfigError::Invalid(format!("invalid shared_buffers \"{shown}\""));
    let bytes = match value {
        SizeValue::Number(n) => n.checked_mul(BLOCK_BYTES),
        SizeValue::Text(text) => {
            let t = text.trim();
            let digits_end = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
            let (num, unit) = t.split_at(digits_end);
            let n: u64 = num.parse().map_err(|_| invalid())?;
            let mult = match unit.trim() {
                "" => BLOCK_BYTES,
                "kB" | "KB" | "kb" => 1 << 10,
                "MB" | "mb" => 1 << 20,
                "GB" | "gb" => 1 << 30,
                _ => return Err(invalid()),
            };
            n.checked_mul(mult)
        }
    };
    let frames = bytes.map(|b| b / BLOCK_BYTES).ok_or_else(invalid)?;
    let frames = usize::try_from(frames).map_err(|_| invalid())?;
    if frames < MIN_FRAMES {
        return Err(ConfigError::Invalid(format!(
            "shared_buffers must be at least 128kB ({MIN_FRAMES} blocks)"
        )));
    }
    Ok(frames)
}

impl Config {
    /// Merges defaults, the file, the environment (`env_data`, the value of
    /// `YUZHU_DATA`) and the command line (in increasing precedence) and
    /// validates the result. The data directory is mandatory.
    pub fn merge(
        file: &FileConfig,
        cli: &Cli,
        env_data: Option<&str>,
    ) -> Result<Self, ConfigError> {
        let mut config = Self::merge_without_data_dir(file, cli)?;
        config.data_directory = cli
            .data_directory
            .clone()
            .or_else(|| file.data_directory.clone())
            .or_else(|| env_data.filter(|s| !s.is_empty()).map(PathBuf::from))
            .ok_or_else(|| {
                ConfigError::Invalid(
                    "no data directory specified: use -D, data_directory in the config file, \
                     or the YUZHU_DATA environment variable"
                        .into(),
                )
            })?;
        Ok(config)
    }

    /// Like [`Config::merge`] but leaves `data_directory` empty when none is
    /// given (for servers started on an existing cluster).
    pub fn merge_without_data_dir(file: &FileConfig, cli: &Cli) -> Result<Self, ConfigError> {
        let d = Self::default();
        let log_level = match cli.log_level.as_deref().or(file.log_level.as_deref()) {
            Some(s) => parse_level(s)?,
            None => d.log_level,
        };
        let shared_buffers = match cli.shared_buffers.clone().map(SizeValue::Text) {
            Some(v) => parse_shared_buffers(&v)?,
            None => match &file.shared_buffers {
                Some(v) => parse_shared_buffers(v)?,
                None => d.shared_buffers,
            },
        };
        let config = Self {
            data_directory: PathBuf::new(),
            listen: cli.listen.or(file.listen).unwrap_or(d.listen),
            port: cli.port.or(file.port).unwrap_or(d.port),
            max_connections: cli
                .max_connections
                .or(file.max_connections)
                .unwrap_or(d.max_connections),
            shared_buffers,
            checkpoint_timeout: cli
                .checkpoint_timeout
                .or(file.checkpoint_timeout)
                .map_or(d.checkpoint_timeout, Duration::from_secs),
            ignore_unclean_shutdown: cli.ignore_unclean_shutdown
                || file.ignore_unclean_shutdown.unwrap_or(false),
            log_level,
        };
        if config.max_connections == 0 {
            return Err(ConfigError::Invalid(
                "max_connections must be at least 1".into(),
            ));
        }
        if config.checkpoint_timeout.is_zero() {
            return Err(ConfigError::Invalid(
                "checkpoint_timeout must be at least 1 second".into(),
            ));
        }
        Ok(config)
    }

    /// Builds the configuration from parsed command-line arguments, loading
    /// the configuration file if `--config` was given and reading
    /// `YUZHU_DATA`.
    pub fn from_cli(cli: &Cli) -> Result<Self, ConfigError> {
        let file = match &cli.config {
            Some(path) => FileConfig::load(path)?,
            None => FileConfig::default(),
        };
        Self::merge(&file, cli, std::env::var("YUZHU_DATA").ok().as_deref())
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

    fn merge(file: &FileConfig, args: &[&str]) -> Result<Config, ConfigError> {
        Config::merge(file, &cli(args), None)
    }

    #[test]
    fn defaults() {
        let c = merge(&FileConfig::default(), &["-D", "/d"]).unwrap();
        let d = Config::default();
        assert_eq!(c.data_directory, PathBuf::from("/d"));
        assert_eq!(c.listen, d.listen);
        assert_eq!(c.listen.to_string(), "127.0.0.1");
        assert_eq!(c.port, 5432);
        assert_eq!(c.max_connections, 100);
        assert_eq!(c.shared_buffers, 16384);
        assert_eq!(c.checkpoint_timeout, Duration::from_secs(300));
        assert!(!c.ignore_unclean_shutdown);
        assert_eq!(c.log_level, tracing::Level::INFO);
    }

    #[test]
    fn cli_arguments() {
        let c = merge(
            &FileConfig::default(),
            &[
                "-D",
                "/data",
                "--listen",
                "0.0.0.0",
                "--port",
                "15432",
                "--max-connections",
                "3",
                "--shared-buffers",
                "1MB",
                "--checkpoint-timeout",
                "10",
                "--ignore-unclean-shutdown",
                "--log-level",
                "debug",
            ],
        )
        .unwrap();
        assert_eq!(c.data_directory, PathBuf::from("/data"));
        assert_eq!(c.listen.to_string(), "0.0.0.0");
        assert_eq!(c.port, 15432);
        assert_eq!(c.max_connections, 3);
        assert_eq!(c.shared_buffers, 128);
        assert_eq!(c.checkpoint_timeout, Duration::from_secs(10));
        assert!(c.ignore_unclean_shutdown);
        assert_eq!(c.log_level, tracing::Level::DEBUG);
    }

    #[test]
    fn data_directory_precedence() {
        let file = FileConfig::parse("data_directory = \"/file\"\n", Path::new("x.toml")).unwrap();
        let with = |args: &[&str], env: Option<&str>| {
            Config::merge(&file, &cli(args), env)
                .unwrap()
                .data_directory
        };
        assert_eq!(with(&["-D", "/cli"], Some("/env")), PathBuf::from("/cli"));
        assert_eq!(with(&[], Some("/env")), PathBuf::from("/file"));
        let none = FileConfig::default();
        let c = Config::merge(&none, &cli(&[]), Some("/env")).unwrap();
        assert_eq!(c.data_directory, PathBuf::from("/env"));
        assert!(Config::merge(&none, &cli(&[]), None).is_err());
        assert!(Config::merge(&none, &cli(&[]), Some("")).is_err());
    }

    #[test]
    fn file_values_and_cli_override() {
        let file = FileConfig::parse(
            "data_directory = \"/d\"\nlisten = \"::1\"\nport = 6000\nmax_connections = 5\n\
             shared_buffers = \"2MB\"\ncheckpoint_timeout = 60\nignore_unclean_shutdown = true\n\
             log_level = \"warn\"\n",
            Path::new("x.toml"),
        )
        .unwrap();
        let c = merge(&file, &[]).unwrap();
        assert_eq!(c.listen.to_string(), "::1");
        assert_eq!(c.port, 6000);
        assert_eq!(c.max_connections, 5);
        assert_eq!(c.shared_buffers, 256);
        assert_eq!(c.checkpoint_timeout, Duration::from_secs(60));
        assert!(c.ignore_unclean_shutdown);
        assert_eq!(c.log_level, tracing::Level::WARN);

        let c = merge(
            &file,
            &[
                "--port",
                "7000",
                "--log-level",
                "error",
                "--shared-buffers",
                "64",
            ],
        )
        .unwrap();
        assert_eq!(c.port, 7000);
        assert_eq!(c.log_level, tracing::Level::ERROR);
        assert_eq!(c.max_connections, 5);
        assert_eq!(c.shared_buffers, 64);
    }

    #[test]
    fn shared_buffers_forms() {
        let text = |s: &str| parse_shared_buffers(&SizeValue::Text(s.into()));
        assert_eq!(text("128MB").unwrap(), 16384);
        assert_eq!(text("1GB").unwrap(), 131_072);
        assert_eq!(text("128kB").unwrap(), 16);
        assert_eq!(text("100").unwrap(), 100);
        assert_eq!(parse_shared_buffers(&SizeValue::Number(32)).unwrap(), 32);
        assert!(text("64kB").is_err());
        assert!(text("0").is_err());
        assert!(text("abc").is_err());
        assert!(text("12XB").is_err());
        assert!(text("").is_err());
        assert!(text("99999999999999999999GB").is_err());
    }

    #[test]
    fn invalid_values() {
        assert!(FileConfig::parse("bogus = 1\n", Path::new("x.toml")).is_err());
        assert!(FileConfig::parse("port = \"abc\"\n", Path::new("x.toml")).is_err());
        let f = FileConfig::default();
        assert!(merge(&f, &["-D", "/d", "--log-level", "loud"]).is_err());
        assert!(merge(&f, &["-D", "/d", "--max-connections", "0"]).is_err());
        assert!(merge(&f, &["-D", "/d", "--checkpoint-timeout", "0"]).is_err());
        assert!(merge(&f, &["-D", "/d", "--shared-buffers", "x"]).is_err());
        assert!(Cli::try_parse_from(["yuzhu-server", "--port", "99999"]).is_err());
        assert!(Cli::try_parse_from(["yuzhu-server", "--listen", "nope"]).is_err());
    }

    #[test]
    fn load_missing_file() {
        let err = FileConfig::load(Path::new("/nonexistent/yuzhu.toml")).unwrap_err();
        assert!(matches!(err, ConfigError::Read { .. }));
    }
}
