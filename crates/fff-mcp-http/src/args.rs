use std::net::IpAddr;
use std::path::PathBuf;

use clap::Parser;

use crate::config::{Config, parse_memory};

/// Long-lived multi-root file finder: one request names a query and a tardir.
#[derive(Debug, Parser)]
#[command(
    name = "fff-mcp-http",
    version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("FFF_GIT_HASH"), ")")
)]
pub struct Args {
    /// TOML config file; command-line flags override its values.
    #[arg(long)]
    pub config: Option<PathBuf>,

    /// Address to bind. Defaults to loopback.
    #[arg(long)]
    pub bind: Option<IpAddr>,

    /// TCP port.
    #[arg(long)]
    pub port: Option<u16>,

    /// Bearer token required for every endpoint except /healthz.
    #[arg(long)]
    pub token: Option<String>,

    /// Index memory budget, e.g. 3G, 512MB or a byte count.
    #[arg(long, value_parser = parse_memory)]
    pub max_memory: Option<u64>,

    /// Longest time a request waits for a scan before returning partial results.
    #[arg(long)]
    pub scan_wait_secs: Option<u64>,

    /// Upper bound on concurrently running initial scans.
    #[arg(long)]
    pub max_concurrent_scans: Option<usize>,

    /// How often the background eviction tick measures memory.
    #[arg(long)]
    pub evict_interval_secs: Option<u64>,

    /// Disable the filesystem watcher; indexes stay at their initial snapshot.
    #[arg(long)]
    pub no_watch: bool,

    /// Follow symlinks while walking.
    #[arg(long)]
    pub follow_symlinks: bool,

    /// Frecency database path. Disabled when omitted.
    #[arg(long)]
    pub frecency_db: Option<PathBuf>,

    /// Log file path-shape hint; each launch writes a fresh timestamped sibling.
    #[arg(long)]
    pub log_file: Option<PathBuf>,

    /// Log level (trace, debug, info, warn, error).
    #[arg(long)]
    pub log_level: Option<String>,

    /// Exit after this many idle seconds. 0 disables.
    #[arg(long)]
    pub idle_timeout_secs: Option<u64>,

    /// Extra gitignore-style skip pattern, repeatable.
    #[arg(long = "skip", value_name = "PATTERN")]
    pub skip: Vec<String>,

    /// Drop the built-in .svn/, .hg/ and CVS/ skip rules.
    #[arg(long)]
    pub no_default_skips: bool,

    /// Do not read .gitignore rules and skip Git integration.
    #[arg(long)]
    pub no_git: bool,

    /// Do not read .ignore files.
    #[arg(long)]
    pub no_ignore_files: bool,

    /// Test-only: expose POST /test/evict.
    #[arg(long, hide = true)]
    pub test_hook: bool,
}

impl Args {
    /// Config file first, then every flag given on the command line.
    pub fn into_config(self) -> Result<Config, String> {
        let mut config = match &self.config {
            Some(path) => Config::from_file(path)?,
            None => Config::default(),
        };
        self.apply_to(&mut config);
        Ok(config)
    }

    fn apply_to(self, config: &mut Config) {
        if let Some(bind) = self.bind {
            config.bind = bind;
        }
        if let Some(port) = self.port {
            config.port = port;
        }
        if let Some(token) = self.token {
            config.token = Some(token);
        }
        if let Some(bytes) = self.max_memory {
            config.max_memory_bytes = bytes;
        }
        if let Some(secs) = self.scan_wait_secs {
            config.scan_wait = std::time::Duration::from_secs(secs);
        }
        if let Some(scans) = self.max_concurrent_scans {
            config.max_concurrent_scans = scans;
        }
        if let Some(secs) = self.evict_interval_secs {
            config.evict_interval = std::time::Duration::from_secs(secs);
        }
        if self.no_watch {
            config.watch = false;
        }
        if self.follow_symlinks {
            config.follow_symlinks = true;
        }
        if let Some(path) = self.frecency_db {
            config.frecency_db = Some(path);
        }
        if let Some(path) = self.log_file {
            config.log_file = Some(path);
        }
        if let Some(level) = self.log_level {
            config.log_level = Some(level);
        }
        if let Some(secs) = self.idle_timeout_secs {
            config.idle_timeout = std::time::Duration::from_secs(secs);
        }
        config.skip.extend(self.skip);
        if self.no_default_skips {
            config.default_skips = false;
        }
        if self.no_git {
            config.disable_git = true;
        }
        if self.no_ignore_files {
            config.disable_ignore_files = true;
        }
        if self.test_hook {
            config.test_hook = true;
        }

        config.max_memory_bytes = config.max_memory_bytes.max(parse_memory("1M").unwrap());
        config.max_concurrent_scans = config.max_concurrent_scans.max(1);
        config.evict_interval = config.evict_interval.max(std::time::Duration::from_secs(1));
    }
}

/// Flags without the config file; `--config` needs IO, so prefer `into_config`.
impl From<Args> for Config {
    fn from(args: Args) -> Self {
        let mut config = Self::default();
        args.apply_to(&mut config);
        config
    }
}

/// Default log file hint, same layout as `fff-mcp`.
pub fn default_log_file() -> PathBuf {
    let base = absolute_env("XDG_STATE_HOME")
        .or_else(|| absolute_env("LOCALAPPDATA"))
        .unwrap_or_else(std::env::temp_dir);
    base.join("fff").join("fff_mcp_http.log")
}

fn absolute_env(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .map(PathBuf::from)
        .filter(|value| value.is_absolute())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{
        DEFAULT_CONCURRENT_SCANS, DEFAULT_EVICT_INTERVAL_SECS, DEFAULT_IDLE_TIMEOUT_SECS,
        DEFAULT_PORT, DEFAULT_SCAN_WAIT_SECS,
    };

    #[test]
    fn default_log_file_is_absolute_and_isolated() {
        let path = default_log_file();
        assert!(path.is_absolute(), "{}", path.display());
        assert_eq!(path.file_name().unwrap(), "fff_mcp_http.log");
        assert_eq!(path.parent().unwrap().file_name().unwrap(), "fff");
    }

    #[test]
    fn cli_parses_into_config() {
        let args = Args::parse_from(["fff-mcp-http", "--port", "9999", "--no-watch"]);
        let config: Config = args.into();
        assert_eq!(config.port, 9999);
        assert!(!config.watch);
        assert_eq!(config.max_memory_bytes, crate::config::DEFAULT_MAX_MEMORY);
    }

    #[test]
    fn cli_survives_without_flags() {
        let config = Args::parse_from(["fff-mcp-http"])
            .into_config()
            .expect("default config");
        assert_eq!(config.port, DEFAULT_PORT);
        assert_eq!(config.scan_wait.as_secs(), DEFAULT_SCAN_WAIT_SECS);
        assert_eq!(config.max_concurrent_scans, DEFAULT_CONCURRENT_SCANS);
        assert_eq!(config.evict_interval.as_secs(), DEFAULT_EVICT_INTERVAL_SECS);
        assert_eq!(config.idle_timeout.as_secs(), DEFAULT_IDLE_TIMEOUT_SECS);
        assert!(config.default_skips);
        assert!(config.skip.is_empty());
    }

    #[test]
    fn cli_skip_flags_append_and_switch_defaults_off() {
        let args = Args::parse_from([
            "fff-mcp-http",
            "--skip",
            "*.bak",
            "--skip",
            "cache/",
            "--no-default-skips",
            "--no-git",
            "--no-ignore-files",
        ]);
        let config = args.into_config().unwrap();
        assert_eq!(config.skip, vec!["*.bak".to_string(), "cache/".to_string()]);
        assert!(!config.default_skips);
        assert!(config.disable_git);
        assert!(config.disable_ignore_files);
    }
}
