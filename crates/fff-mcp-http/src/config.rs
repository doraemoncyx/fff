use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::time::Duration;

pub const DEFAULT_PORT: u16 = 8737;
pub const DEFAULT_MAX_MEMORY: u64 = 3 * 1024 * 1024 * 1024;
pub const DEFAULT_SCAN_WAIT_SECS: u64 = 20;
pub const DEFAULT_CONCURRENT_SCANS: usize = 2;
pub const DEFAULT_EVICT_INTERVAL_SECS: u64 = 30;
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 3600;

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
        }
    }
}

impl Config {
    pub fn is_loopback(&self) -> bool {
        self.bind.is_loopback()
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
}
