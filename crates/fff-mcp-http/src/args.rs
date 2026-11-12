use std::net::IpAddr;
use std::path::PathBuf;

use clap::Parser;

use crate::config::{
    Config, DEFAULT_CONCURRENT_SCANS, DEFAULT_EVICT_INTERVAL_SECS, DEFAULT_IDLE_TIMEOUT_SECS,
    DEFAULT_PORT, DEFAULT_SCAN_WAIT_SECS, parse_memory,
};

/// Long-lived multi-root file finder: one request names a query and a tardir.
#[derive(Debug, Parser)]
#[command(
    name = "fff-mcp-http",
    version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("FFF_GIT_HASH"), ")")
)]
pub struct Args {
    /// Address to bind. Defaults to loopback.
    #[arg(long, default_value = "127.0.0.1")]
    pub bind: IpAddr,

    /// TCP port.
    #[arg(long, default_value_t = DEFAULT_PORT)]
    pub port: u16,

    /// Bearer token required for every endpoint except /healthz.
    #[arg(long)]
    pub token: Option<String>,

    /// Index memory budget, e.g. 3G, 512MB or a byte count.
    #[arg(long, default_value = "3G", value_parser = parse_memory)]
    pub max_memory: u64,

    /// Longest time a request waits for a scan before returning partial results.
    #[arg(long, default_value_t = DEFAULT_SCAN_WAIT_SECS)]
    pub scan_wait_secs: u64,

    /// Upper bound on concurrently running initial scans.
    #[arg(long, default_value_t = DEFAULT_CONCURRENT_SCANS)]
    pub max_concurrent_scans: usize,

    /// How often the background eviction tick measures memory.
    #[arg(long, default_value_t = DEFAULT_EVICT_INTERVAL_SECS)]
    pub evict_interval_secs: u64,

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
    #[arg(long, default_value_t = DEFAULT_IDLE_TIMEOUT_SECS)]
    pub idle_timeout_secs: u64,

    /// Test-only: expose POST /test/evict.
    #[arg(long, hide = true)]
    pub test_hook: bool,
}

impl From<Args> for Config {
    fn from(args: Args) -> Self {
        Self {
            bind: args.bind,
            port: args.port,
            token: args.token,
            max_memory_bytes: args.max_memory.max(parse_memory("1M").unwrap()),
            scan_wait: std::time::Duration::from_secs(args.scan_wait_secs),
            max_concurrent_scans: args.max_concurrent_scans.max(1),
            evict_interval: std::time::Duration::from_secs(args.evict_interval_secs.max(1)),
            watch: !args.no_watch,
            follow_symlinks: args.follow_symlinks,
            frecency_db: args.frecency_db,
            log_file: args.log_file,
            log_level: args.log_level,
            idle_timeout: std::time::Duration::from_secs(args.idle_timeout_secs),
            test_hook: args.test_hook,
            ..Config::default()
        }
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
}
