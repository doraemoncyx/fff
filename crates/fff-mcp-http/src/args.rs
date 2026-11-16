use std::net::IpAddr;
use std::path::PathBuf;

use clap::Parser;

use crate::config::{Config, parse_memory};

#[derive(Debug, Parser)]
#[command(
    name = "fff-mcp-http",
    version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("FFF_GIT_HASH"), ")"),
    about = "Long-lived multi-root file finder over HTTP: every request names a query and a tardir",
    after_help = "Every option is also a TOML key in the --config file (hyphens become\n\
                  underscores, boolean flags are plain booleans). Command-line flags win.\n\n\
                  --skip, --no-default-skips, --no-git and --no-ignore-files need the zlob\n\
                  walker; a ripgrep-only build refuses to start with them.\n\n\
                  Examples:\n  \
                  fff-mcp-http --port 8737 --max-memory 3G\n  \
                  fff-mcp-http --bind 0.0.0.0 --token <secret>\n  \
                  fff-mcp-http --config ./fff-mcp-http.toml --log-level debug"
)]
pub struct Args {
    #[arg(
        long,
        value_name = "FILE",
        help = "TOML settings file; command-line flags override its keys"
    )]
    pub config: Option<PathBuf>,

    #[arg(
        long,
        value_name = "IP",
        help = "Listen address, IPv4/IPv6 literal (default: 127.0.0.1); non-loopback needs --token"
    )]
    pub bind: Option<IpAddr>,

    #[arg(
        long,
        value_name = "PORT",
        help = "TCP port, 0 = OS-picked (default: 8737)"
    )]
    pub port: Option<u16>,

    #[arg(
        long,
        value_name = "TOKEN",
        help = "Bearer token for every endpoint except /healthz; required for non-loopback --bind"
    )]
    pub token: Option<String>,

    #[arg(
        long,
        value_name = "SIZE",
        value_parser = parse_memory,
        help = "Memory budget for all indexes (default: 3G); K/M/G/T are binary, 1.5G is allowed"
    )]
    pub max_memory: Option<u64>,

    #[arg(
        long,
        value_name = "SECS",
        help = "Seconds to wait for a tardir's first scan before answering partially (default: 20)"
    )]
    pub scan_wait_secs: Option<u64>,

    #[arg(
        long,
        value_name = "N",
        help = "Initial scans running at once, the rest queue (default: 2, min 1)"
    )]
    pub max_concurrent_scans: Option<usize>,

    #[arg(
        long,
        value_name = "SECS",
        help = "Seconds between memory checks that may evict idle Roots (default: 30, min 1)"
    )]
    pub evict_interval_secs: Option<u64>,

    #[arg(
        long,
        help = "Do not watch the filesystem; indexes stay at their initial scan"
    )]
    pub no_watch: bool,

    #[arg(
        long,
        help = "Follow symlinks while walking (default: off, may loop or duplicate paths)"
    )]
    pub follow_symlinks: bool,

    #[arg(
        long,
        help = "Do not expand files inside symlinked directories (default: on, capped at 50k files)"
    )]
    pub no_symlink_expand: bool,

    #[arg(
        long,
        help = "Build the bigram content index after each scan, grep's prefilter (default: on)"
    )]
    pub content_indexing: bool,

    #[arg(
        long,
        conflicts_with = "content_indexing",
        help = "Skip the content index: grep reads every file (less RAM, slower grep)"
    )]
    pub no_content_indexing: bool,

    #[arg(
        long,
        value_name = "PATH",
        help = "Frecency database directory (default: none, frecency scoring off)"
    )]
    pub frecency_db: Option<PathBuf>,

    #[arg(
        long,
        value_name = "PATH",
        help = "Snapshot environment directory (default: <cache dir>/fff/snapshot-db)"
    )]
    pub snapshot_db: Option<PathBuf>,

    #[arg(
        long,
        help = "Do not write or read a Snapshot of any Root's Content index (default: Snapshot on)"
    )]
    pub no_snapshot: bool,

    #[arg(
        long,
        value_name = "MB",
        help = "Snapshot disk budget in MB, 0 = no Snapshot (default: 512)"
    )]
    pub snapshot_budget_mb: Option<u64>,

    #[arg(
        long,
        help = "Empty the Snapshot before opening it (default: off, existing rows kept)"
    )]
    pub snapshot_reset: bool,

    #[arg(
        long,
        value_name = "PATH",
        help = "Log path hint; each launch writes <stem>+<timestamp>+<pid>.log (default: <state dir>/fff/)"
    )]
    pub log_file: Option<PathBuf>,

    #[arg(
        long,
        value_name = "LEVEL",
        help = "Log verbosity: trace, debug, info, warn or error (default: info; RUST_LOG wins)"
    )]
    pub log_level: Option<String>,

    #[arg(
        long,
        value_name = "SECS",
        help = "Exit after SECS without a request; /healthz is not activity (default: 3600, 0 = never)"
    )]
    pub idle_timeout_secs: Option<u64>,

    #[arg(
        long = "skip",
        value_name = "PATTERN",
        help = "Extra gitignore-style skip pattern, repeatable (applies to new Roots)"
    )]
    pub skip: Vec<String>,

    #[arg(long, help = "Drop the built-in .svn/, .hg/ and CVS/ skip rules")]
    pub no_default_skips: bool,

    #[arg(long, help = "Ignore .gitignore rules and skip git integration")]
    pub no_git: bool,

    #[arg(long, help = "Ignore .ignore files")]
    pub no_ignore_files: bool,

    #[arg(long, hide = true, help = "Test-only: expose POST /test/evict")]
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
        if self.no_symlink_expand {
            config.expand_symlink_dirs = false;
        }
        if self.content_indexing {
            config.content_indexing = true;
        }
        if self.no_content_indexing {
            config.content_indexing = false;
        }
        if let Some(path) = self.frecency_db {
            config.frecency_db = Some(path);
        }
        if let Some(path) = self.snapshot_db {
            config.snapshot_db = Some(path);
            config.snapshot = true;
        }
        if self.no_snapshot {
            config.snapshot = false;
        }
        if let Some(mb) = self.snapshot_budget_mb {
            config.snapshot_budget_mb = mb;
        }
        if self.snapshot_reset {
            config.snapshot_reset = true;
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
        if config.snapshot_budget_mb == 0 {
            config.snapshot = false;
        }
        if config.snapshot && config.snapshot_db.is_none() {
            config.snapshot_db = default_snapshot_db();
        }
    }
}

// Flags without the config file; `--config` needs IO, so prefer `into_config`.
impl From<Args> for Config {
    fn from(args: Args) -> Self {
        let mut config = Self::default();
        args.apply_to(&mut config);
        config
    }
}

/// Default log file hint, same layout as `fff-mcp`.
pub fn default_log_file() -> PathBuf {
    let base = default_log_base();
    base.join("fff").join("fff_mcp_http.log")
}

/// Default Snapshot environment, one shared directory in the platform cache.
pub fn default_snapshot_db() -> Option<PathBuf> {
    dirs::cache_dir().map(|base| base.join("fff").join("snapshot-db"))
}

#[cfg(windows)]
fn default_log_base() -> PathBuf {
    absolute_env("LOCALAPPDATA")
        .or_else(|| absolute_env("USERPROFILE").map(|home| home.join("AppData").join("Local")))
        .unwrap_or_else(std::env::temp_dir)
}

#[cfg(not(windows))]
fn default_log_base() -> PathBuf {
    absolute_env("XDG_STATE_HOME")
        .or_else(|| absolute_env("HOME").map(|home| home.join(".local").join("state")))
        .unwrap_or_else(std::env::temp_dir)
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
        DEFAULT_PORT, DEFAULT_SCAN_WAIT_SECS, DEFAULT_SNAPSHOT_BUDGET_MB,
    };

    #[test]
    fn every_visible_option_documents_itself() {
        use clap::CommandFactory;
        for arg in Args::command().get_arguments() {
            if arg.is_hide_set() {
                continue;
            }
            assert!(
                arg.get_help()
                    .is_some_and(|help| !help.to_string().trim().is_empty()),
                "--{} has no help text",
                arg.get_long()
                    .map(str::to_string)
                    .unwrap_or_else(|| arg.get_id().to_string())
            );
        }
    }

    #[test]
    fn the_snapshot_options_state_their_unit_and_default() {
        use clap::CommandFactory;
        let command = Args::command();
        for flag in [
            "snapshot-db",
            "no-snapshot",
            "snapshot-budget-mb",
            "snapshot-reset",
        ] {
            let arg = command
                .get_arguments()
                .find(|arg| arg.get_long() == Some(flag))
                .unwrap_or_else(|| panic!("--{flag} is missing"));
            let help = arg.get_help().expect("help text").to_string();
            assert!(
                help.contains("default"),
                "--{flag} does not state its default: {help}"
            );
        }
        let budget = command
            .get_arguments()
            .find(|arg| arg.get_long() == Some("snapshot-budget-mb"))
            .and_then(|arg| arg.get_help())
            .map(|help| help.to_string())
            .unwrap_or_default();
        assert!(
            budget.contains("MB"),
            "--snapshot-budget-mb has no unit: {budget}"
        );
        assert!(
            budget.contains("512"),
            "--snapshot-budget-mb hides its default: {budget}"
        );
    }

    #[test]
    fn snapshot_flags_parse_into_config() {
        let dir = std::env::temp_dir().join("fff-snapshot-flag-test");
        let args = Args::parse_from([
            "fff-mcp-http",
            "--snapshot-db",
            dir.to_str().unwrap(),
            "--snapshot-budget-mb",
            "64",
            "--snapshot-reset",
        ]);
        let config = args.into_config().unwrap();
        assert!(config.snapshot);
        assert_eq!(config.snapshot_db.as_deref(), Some(dir.as_path()));
        assert_eq!(config.snapshot_budget_mb, 64);
        assert!(config.snapshot_reset);
        assert_eq!(config.snapshot_path(), Some(dir.as_path()));
    }

    #[test]
    fn no_snapshot_turns_it_off_and_names_no_directory() {
        let config = Args::parse_from(["fff-mcp-http", "--no-snapshot"])
            .into_config()
            .unwrap();
        assert!(!config.snapshot);
        assert!(config.snapshot_db.is_none());
        assert!(config.snapshot_path().is_none());
    }

    #[test]
    fn a_zero_budget_behaves_like_no_snapshot() {
        let config = Args::parse_from(["fff-mcp-http", "--snapshot-budget-mb", "0"])
            .into_config()
            .unwrap();
        assert_eq!(config.snapshot_budget_mb, 0);
        assert!(!config.snapshot);
        assert!(config.snapshot_db.is_none());
    }

    #[test]
    fn the_default_snapshot_directory_is_a_cache_path() {
        let config = Args::parse_from(["fff-mcp-http"]).into_config().unwrap();
        assert_eq!(config.snapshot_budget_mb, DEFAULT_SNAPSHOT_BUDGET_MB);
        assert!(!config.snapshot_reset);
        let path = config.snapshot_db.expect("default snapshot directory");
        assert!(path.is_absolute(), "{}", path.display());
        assert_eq!(path.file_name().unwrap(), "snapshot-db");
        assert_eq!(path.parent().unwrap().file_name().unwrap(), "fff");
    }

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

    #[test]
    fn content_indexing_is_on_unless_the_flag_says_otherwise() {
        let default = Args::parse_from(["fff-mcp-http"]).into_config().unwrap();
        assert!(default.content_indexing);

        let off = Args::parse_from(["fff-mcp-http", "--no-content-indexing"])
            .into_config()
            .unwrap();
        assert!(!off.content_indexing);

        let forced = Args::parse_from(["fff-mcp-http", "--content-indexing"])
            .into_config()
            .unwrap();
        assert!(forced.content_indexing);
    }

    #[test]
    fn the_two_content_indexing_flags_conflict() {
        let parsed = Args::try_parse_from([
            "fff-mcp-http",
            "--content-indexing",
            "--no-content-indexing",
        ]);
        assert!(parsed.is_err(), "both flags must be rejected");
    }
}
