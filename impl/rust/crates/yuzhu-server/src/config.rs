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
use yuzhu_core::engine::DEFAULT_MAX_WAL_SIZE;

/// Size of one buffer frame (a page), the unit of a bare `shared_buffers`.
const BLOCK_BYTES: u64 = 8192;
/// Minimum for `shared_buffers` (512kB). PostgreSQL allows 128kB, but a B+Tree build pins up to 33
/// buffers at once and a split pins `3h + 1`, so fewer frames fail with "no unpinned buffers".
const MIN_FRAMES: usize = 64;
/// Smallest accepted `max_wal_size` (one minimum WAL segment).
const MIN_MAX_WAL_SIZE: u64 = 2 << 20;

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
    /// Start a checkpoint when this much WAL has accumulated, e.g. `1GB`
    /// (a bare number counts MB, like PostgreSQL). Default 1GB.
    #[arg(long, value_name = "SIZE")]
    pub max_wal_size: Option<String>,
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
    pub max_wal_size: Option<SizeValue>,
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
    /// Bytes of WAL that trigger a checkpoint.
    pub max_wal_size: u64,
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
            max_wal_size: DEFAULT_MAX_WAL_SIZE,
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

/// Parses a size into bytes. A bare number counts `bare_unit` bytes; `kB`,
/// `MB` and `GB` suffixes are accepted.
fn parse_size_bytes(name: &str, value: &SizeValue, bare_unit: u64) -> Result<u64, ConfigError> {
    let shown = match value {
        SizeValue::Number(n) => n.to_string(),
        SizeValue::Text(s) => s.clone(),
    };
    let invalid = || ConfigError::Invalid(format!("invalid {name} \"{shown}\""));
    let bytes = match value {
        SizeValue::Number(n) => n.checked_mul(bare_unit),
        SizeValue::Text(text) => {
            let t = text.trim();
            let digits_end = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
            let (num, unit) = t.split_at(digits_end);
            let n: u64 = num.parse().map_err(|_| invalid())?;
            let mult = match unit.trim() {
                "" => bare_unit,
                "kB" | "KB" | "kb" => 1 << 10,
                "MB" | "mb" => 1 << 20,
                "GB" | "gb" => 1 << 30,
                _ => return Err(invalid()),
            };
            n.checked_mul(mult)
        }
    };
    bytes.ok_or_else(invalid)
}

/// Parses a `shared_buffers` value into a number of frames. A bare number
/// counts 8kB blocks (like PostgreSQL); `kB`, `MB` and `GB` suffixes are
/// accepted.
pub fn parse_shared_buffers(value: &SizeValue) -> Result<usize, ConfigError> {
    let bytes = parse_size_bytes("shared_buffers", value, BLOCK_BYTES)?;
    let frames = usize::try_from(bytes / BLOCK_BYTES)
        .map_err(|_| ConfigError::Invalid("shared_buffers is too large".into()))?;
    if frames < MIN_FRAMES {
        return Err(ConfigError::Invalid(format!(
            "shared_buffers must be at least 512kB ({MIN_FRAMES} blocks)"
        )));
    }
    Ok(frames)
}

/// Parses a `max_wal_size` value into bytes. A bare number counts MB (like
/// PostgreSQL); the minimum is 2MB.
pub fn parse_max_wal_size(value: &SizeValue) -> Result<u64, ConfigError> {
    let bytes = parse_size_bytes("max_wal_size", value, 1 << 20)?;
    if bytes < MIN_MAX_WAL_SIZE {
        return Err(ConfigError::Invalid(
            "max_wal_size must be at least 2MB".into(),
        ));
    }
    Ok(bytes)
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
        let max_wal_size = match cli.max_wal_size.clone().map(SizeValue::Text) {
            Some(v) => parse_max_wal_size(&v)?,
            None => match &file.max_wal_size {
                Some(v) => parse_max_wal_size(v)?,
                None => d.max_wal_size,
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
            max_wal_size,
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
        assert_eq!(c.max_wal_size, DEFAULT_MAX_WAL_SIZE);
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
                "--max-wal-size",
                "64MB",
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
        assert_eq!(c.max_wal_size, 64 << 20);
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
             shared_buffers = \"2MB\"\ncheckpoint_timeout = 60\nmax_wal_size = 32\n\
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
        assert_eq!(c.max_wal_size, 32 << 20);
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
        assert_eq!(text("512kB").unwrap(), 64);
        assert!(text("128kB").is_err());
        assert_eq!(text("100").unwrap(), 100);
        assert_eq!(parse_shared_buffers(&SizeValue::Number(80)).unwrap(), 80);
        assert!(text("64kB").is_err());
        assert!(text("0").is_err());
        assert!(text("abc").is_err());
        assert!(text("12XB").is_err());
        assert!(text("").is_err());
        assert!(text("99999999999999999999GB").is_err());
    }

    #[test]
    fn max_wal_size_forms() {
        let text = |s: &str| parse_max_wal_size(&SizeValue::Text(s.into()));
        assert_eq!(text("1GB").unwrap(), 1 << 30);
        assert_eq!(text("512").unwrap(), 512 << 20);
        assert_eq!(text("2048kB").unwrap(), 2 << 20);
        assert_eq!(parse_max_wal_size(&SizeValue::Number(4)).unwrap(), 4 << 20);
        assert!(text("1MB").is_err());
        assert!(text("x").is_err());
        assert!(text("99999999999999999999GB").is_err());
    }

    #[test]
    fn removed_option_is_rejected() {
        assert!(Cli::try_parse_from(["yuzhu-server", "--ignore-unclean-shutdown"]).is_err());
        assert!(
            FileConfig::parse("ignore_unclean_shutdown = true\n", Path::new("x.toml")).is_err()
        );
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
