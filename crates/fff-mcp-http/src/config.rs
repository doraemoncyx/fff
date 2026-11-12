use std::net::{IpAddr, Ipv4Addr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use fff::FilePickerIgnoreOptions;
use serde::Deserialize;

pub const NON_ZLOB_MESSAGE: &str = "skip patterns and the no-git/no-ignore-files switches need the zlob walker; \
     rebuild fff-mcp-http with --features zlob";

pub const DEFAULT_PORT: u16 = 8737;
pub const DEFAULT_MAX_MEMORY: u64 = 3 * 1024 * 1024 * 1024;
pub const DEFAULT_SCAN_WAIT_SECS: u64 = 20;
pub const DEFAULT_CONCURRENT_SCANS: usize = 2;
pub const DEFAULT_EVICT_INTERVAL_SECS: u64 = 30;
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 3600;

// rmcp's own loopback allowlist; config entries are added to it.
const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

pub const TOKEN_HINT: &str = "generate one with `openssl rand -hex 24`, \
     or in PowerShell `[guid]::NewGuid().ToString('N')`, then pass --token <value>";

#[derive(Debug, Clone)]
pub struct Config {
    pub bind: IpAddr,
    pub port: u16,
    pub token: Option<String>,
    pub max_memory_bytes: u64,
    pub scan_wait: Duration,
    pub max_concurrent_scans: usize,
    pub evict_interval: Duration,
    pub watch: bool,
    pub follow_symlinks: bool,
    pub frecency_db: Option<PathBuf>,
    pub log_file: Option<PathBuf>,
    pub log_level: Option<String>,
    pub idle_timeout: Duration,
    pub skip: Vec<String>,
    pub default_skips: bool,
    pub disable_git: bool,
    pub disable_ignore_files: bool,
    pub allowed_hosts: Vec<String>,
    /// Test-only: expose `POST /test/evict`.
    pub test_hook: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port: DEFAULT_PORT,
            token: None,
            max_memory_bytes: DEFAULT_MAX_MEMORY,
            scan_wait: Duration::from_secs(DEFAULT_SCAN_WAIT_SECS),
            max_concurrent_scans: DEFAULT_CONCURRENT_SCANS,
            evict_interval: Duration::from_secs(DEFAULT_EVICT_INTERVAL_SECS),
            watch: true,
            follow_symlinks: false,
            frecency_db: None,
            log_file: None,
            log_level: None,
            idle_timeout: Duration::from_secs(DEFAULT_IDLE_TIMEOUT_SECS),
            skip: Vec::new(),
            default_skips: true,
            disable_git: false,
            disable_ignore_files: false,
            allowed_hosts: Vec::new(),
            test_hook: false,
        }
    }
}

impl Config {
    pub fn is_loopback(&self) -> bool {
        self.bind.is_loopback()
    }

    /// The token to require, ignoring a blank one.
    pub fn bearer_token(&self) -> Option<&str> {
        self.token
            .as_deref()
            .map(str::trim)
            .filter(|token| !token.is_empty())
    }

    /// Refuse a network-reachable bind that has no token to protect it.
    pub fn check_access(&self) -> Result<(), String> {
        if self.is_loopback() || self.bearer_token().is_some() {
            return Ok(());
        }
        Err(format!(
            "refusing to bind {} without --token: the endpoint would be reachable from the network; {TOKEN_HINT}",
            self.bind
        ))
    }

    /// rmcp's loopback hosts plus any extra names from the config file.
    pub fn resolved_allowed_hosts(&self) -> Vec<String> {
        let mut hosts: Vec<String> = LOOPBACK_HOSTS
            .iter()
            .map(|host| (*host).to_string())
            .collect();
        for host in &self.allowed_hosts {
            let host = host.trim();
            if !host.is_empty() && !hosts.iter().any(|known| known == host) {
                hosts.push(host.to_string());
            }
        }
        hosts
    }

    /// Default skip rules for trees where VCS metadata was copied in.
    pub fn skip_rules(&self) -> Vec<String> {
        let mut rules = Vec::new();
        if self.default_skips {
            rules.extend([".svn/".to_string(), ".hg/".to_string(), "CVS/".to_string()]);
        }
        rules.extend(self.skip.iter().cloned());
        rules
    }

    /// Skip rules plus git/ignore-file switches, ready for a walker.
    pub fn ignore_options(&self) -> Result<FilePickerIgnoreOptions, String> {
        let rules: Arc<[String]> = self.skip_rules().into();
        let has_switches = self.disable_git || self.disable_ignore_files;
        if !cfg!(feature = "zlob") && (!rules.is_empty() || has_switches) {
            return Err(NON_ZLOB_MESSAGE.to_string());
        }
        Ok(FilePickerIgnoreOptions {
            extra_ignore: rules,
            disable_git: self.disable_git,
            disable_ignore_files: self.disable_ignore_files,
        })
    }

    /// Defaults, then the TOML file at `path`.
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("cannot read config file {}: {e}", path.display()))?;
        let file: ConfigFile = toml::from_str(&text)
            .map_err(|e| format!("cannot parse config file {}: {e}", path.display()))?;
        let mut config = Self::default();
        file.apply(&mut config)
            .map_err(|e| format!("config file {}: {e}", path.display()))?;
        Ok(config)
    }
}

// Keys accepted by `--config`. Every key is optional; absent means default.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ConfigFile {
    skip: Option<Vec<String>>,
    max_memory: Option<String>,
    bind: Option<String>,
    port: Option<u16>,
    token: Option<String>,
    scan_wait_secs: Option<u64>,
    max_concurrent_scans: Option<usize>,
    evict_interval_secs: Option<u64>,
    no_watch: Option<bool>,
    follow_symlinks: Option<bool>,
    frecency_db: Option<PathBuf>,
    log_file: Option<PathBuf>,
    log_level: Option<String>,
    default_skips: Option<bool>,
    disable_git: Option<bool>,
    disable_ignore_files: Option<bool>,
    idle_timeout_secs: Option<u64>,
    allowed_hosts: Option<Vec<String>>,
}

impl ConfigFile {
    fn apply(self, config: &mut Config) -> Result<(), String> {
        if let Some(skip) = self.skip {
            config.skip.extend(skip);
        }
        if let Some(raw) = self.max_memory {
            config.max_memory_bytes = parse_memory(&raw)?;
        }
        if let Some(raw) = self.bind {
            config.bind = raw
                .trim()
                .parse()
                .map_err(|e| format!("invalid bind: {e}"))?;
        }
        if let Some(port) = self.port {
            config.port = port;
        }
        if let Some(token) = self.token {
            config.token = Some(token);
        }
        if let Some(secs) = self.scan_wait_secs {
            config.scan_wait = Duration::from_secs(secs);
        }
        if let Some(scans) = self.max_concurrent_scans {
            config.max_concurrent_scans = scans;
        }
        if let Some(secs) = self.evict_interval_secs {
            config.evict_interval = Duration::from_secs(secs);
        }
        if let Some(no_watch) = self.no_watch {
            config.watch = !no_watch;
        }
        if let Some(follow) = self.follow_symlinks {
            config.follow_symlinks = follow;
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
        if let Some(defaults) = self.default_skips {
            config.default_skips = defaults;
        }
        if let Some(disable) = self.disable_git {
            config.disable_git = disable;
        }
        if let Some(disable) = self.disable_ignore_files {
            config.disable_ignore_files = disable;
        }
        if let Some(secs) = self.idle_timeout_secs {
            config.idle_timeout = Duration::from_secs(secs);
        }
        if let Some(hosts) = self.allowed_hosts {
            config.allowed_hosts.extend(hosts);
        }
        Ok(())
    }
}

/// Parse `3G`, `3GB`, `512MB`, `1536M` or a bare byte count.
pub fn parse_memory(raw: &str) -> Result<u64, String> {
    let text = raw.trim().to_ascii_uppercase();
    if text.is_empty() {
        return Err("empty memory size".into());
    }
    let split = text
        .find(|c: char| !(c.is_ascii_digit() || c == '.'))
        .unwrap_or(text.len());
    let (digits, unit) = text.split_at(split);
    let multiplier: u64 = match unit.trim() {
        "" | "B" => 1,
        "K" | "KB" => 1024,
        "M" | "MB" => 1024 * 1024,
        "G" | "GB" => 1024 * 1024 * 1024,
        "T" | "TB" => 1024 * 1024 * 1024 * 1024,
        other => return Err(format!("unknown memory unit: {other}")),
    };
    let value: f64 = digits
        .parse()
        .map_err(|_| format!("invalid memory size: {raw}"))?;
    if !value.is_finite() || value < 0.0 {
        return Err(format!("invalid memory size: {raw}"));
    }
    Ok((value * multiplier as f64) as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_suffixes_are_binary() {
        assert_eq!(parse_memory("3G").unwrap(), 3 * 1024 * 1024 * 1024);
        assert_eq!(parse_memory("3GB").unwrap(), 3 * 1024 * 1024 * 1024);
        assert_eq!(parse_memory("512MB").unwrap(), 512 * 1024 * 1024);
        assert_eq!(parse_memory("1024").unwrap(), 1024);
        assert_eq!(parse_memory("1.5G").unwrap(), 1610612736);
        assert!(parse_memory("").is_err());
        assert!(parse_memory("lots").is_err());
    }

    #[test]
    fn default_skips_can_be_disabled() {
        let mut config = Config::default();
        assert_eq!(config.skip_rules().len(), 3);
        config.default_skips = false;
        assert!(config.skip_rules().is_empty());
        config.skip.push("*.bak".into());
        assert_eq!(config.skip_rules(), vec!["*.bak".to_string()]);
    }

    #[test]
    fn ignore_options_carry_rules_and_switches() {
        let config = Config {
            skip: vec!["*.bak".into()],
            disable_git: true,
            disable_ignore_files: true,
            ..Config::default()
        };
        let options = config.ignore_options().expect("zlob build accepts skips");
        assert_eq!(
            &*options.extra_ignore,
            &[".svn/", ".hg/", "CVS/", "*.bak"].map(String::from)
        );
        assert!(options.disable_git);
        assert!(options.disable_ignore_files);
    }

    // A ripgrep-only build must refuse skip flags instead of ignoring them.
    #[test]
    fn skip_flags_need_the_zlob_walker() {
        let with_defaults = Config::default();
        let with_skip = Config {
            default_skips: false,
            skip: vec!["*.bak".into()],
            ..Config::default()
        };
        let with_no_git = Config {
            default_skips: false,
            disable_git: true,
            ..Config::default()
        };

        if cfg!(feature = "zlob") {
            assert!(with_defaults.ignore_options().is_ok());
            assert!(with_skip.ignore_options().is_ok());
            assert!(with_no_git.ignore_options().is_ok());
        } else {
            assert!(with_defaults.ignore_options().is_err());
            assert!(with_skip.ignore_options().is_err());
            assert!(with_no_git.ignore_options().is_err());
            let plain = Config {
                default_skips: false,
                ..Config::default()
            };
            assert!(plain.ignore_options().is_ok());
        }
    }

    #[test]
    fn non_loopback_bind_requires_a_token() {
        let open = Config {
            bind: "0.0.0.0".parse().unwrap(),
            ..Config::default()
        };
        let message = open.check_access().unwrap_err();
        assert!(message.contains("--token"), "{message}");
        assert!(message.contains("openssl rand -hex 24"), "{message}");

        let blank = Config {
            token: Some("   ".into()),
            ..open.clone()
        };
        assert!(blank.check_access().is_err());

        let guarded = Config {
            token: Some("s3cret".into()),
            ..open
        };
        assert!(guarded.check_access().is_ok());
        assert!(Config::default().check_access().is_ok());
    }

    #[test]
    fn allowed_hosts_extend_the_loopback_defaults() {
        assert_eq!(
            Config::default().resolved_allowed_hosts(),
            vec!["localhost", "127.0.0.1", "::1"]
        );
        let extended = Config {
            allowed_hosts: vec!["example.test".into(), "127.0.0.1".into()],
            ..Config::default()
        };
        assert_eq!(
            extended.resolved_allowed_hosts(),
            vec!["localhost", "127.0.0.1", "::1", "example.test"]
        );
    }

    #[test]
    fn config_file_extends_allowed_hosts() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("hosts.toml");
        std::fs::write(&path, "allowed_hosts = [\"box.test\"]\n").unwrap();
        let config = Config::from_file(&path).unwrap();
        assert_eq!(config.allowed_hosts, vec!["box.test".to_string()]);
        assert!(
            config
                .resolved_allowed_hosts()
                .contains(&"box.test".to_string())
        );
    }

    #[test]
    fn config_file_overrides_defaults_and_reports_bad_input() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "skip = [\"*.bak\"]\nmax_memory = \"64MB\"\nport = 65000\ndefault_skips = false\n",
        )
        .unwrap();
        let config = Config::from_file(&path).unwrap();
        assert_eq!(config.max_memory_bytes, 64 * 1024 * 1024);
        assert_eq!(config.port, 65000);
        assert!(!config.default_skips);
        assert_eq!(config.skip, vec!["*.bak".to_string()]);

        let missing = Config::from_file(&dir.path().join("absent.toml")).unwrap_err();
        assert!(missing.contains("absent.toml"), "{missing}");

        std::fs::write(&path, "port = ").unwrap();
        let malformed = Config::from_file(&path).unwrap_err();
        assert!(malformed.contains("config.toml"), "{malformed}");

        std::fs::write(&path, "max_memory = \"lots\"\n").unwrap();
        let bad_size = Config::from_file(&path).unwrap_err();
        assert!(bad_size.contains("config.toml"), "{bad_size}");

        std::fs::write(&path, "prot = 1\n").unwrap();
        let unknown = Config::from_file(&path).unwrap_err();
        assert!(unknown.contains("config.toml"), "{unknown}");
    }
}
