use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tracing_appender::non_blocking;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::{EnvFilter, fmt, prelude::*};

// Set once on first init_tracing; doubles as the init-once gate.
static LOG_FILE_PATH: OnceLock<PathBuf> = OnceLock::new();
static CRASH_HOOKS: OnceLock<()> = OnceLock::new();

fn write_crash_report(header: &str, body: &str) {
    let msg = format!(
        "\n=== CRASH (this might NOT BE fff related) {} ===\n{}\n=== CRASH END {} ===\n",
        header, body, header
    );
    let _ = std::io::Write::write_all(&mut std::io::stderr(), msg.as_bytes());
    if let Some(path) = LOG_FILE_PATH.get() {
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .and_then(|mut f| std::io::Write::write_all(&mut f, msg.as_bytes()));
    }
}

// SIGSEGV handler writes a banner to a pre-opened fd (open(2) inside a signal
// handler is unsafe due to path-resolution allocs). Unix only.
#[cfg(unix)]
mod sigsegv {
    use std::os::fd::IntoRawFd;
    use std::path::Path;
    use std::sync::atomic::{AtomicI32, Ordering};

    static LOG_FD: AtomicI32 = AtomicI32::new(-1);

    // Must `create(true)` — this runs before init_tracing opens/creates the
    // writer file, so an append-only open on a non-existent path silently
    // fails, LOG_FD stays -1, and the SIGSEGV banner never reaches the log.
    pub fn set_log_fd(path: &Path) {
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
        {
            let prev = LOG_FD.swap(file.into_raw_fd(), Ordering::Relaxed);
            if prev >= 0 {
                unsafe { libc::close(prev) };
            }
        }
    }

    // Body must be async-signal-safe: write(2), atomic load, signal(2). Nothing else.
    fn handler(_info: &libc::siginfo_t) {
        const BANNER: &[u8] = b"\n=== CRASH SIGSEGV (fff) ===\n\
            fff.nvim's rust extension hit a segfault and is about to die.\n\
            Please file the bug at https://github.com/dmtrKovalenko/fff/issues with this banner attached.\n\
            === CRASH END SIGSEGV ===\n";
        unsafe {
            libc::write(2, BANNER.as_ptr().cast(), BANNER.len());
            let log_fd = LOG_FD.load(Ordering::Relaxed);
            if log_fd >= 0 {
                libc::write(log_fd, BANNER.as_ptr().cast(), BANNER.len());
            }
            // Reset to default so handler return → kernel kills us instead of
            // re-running the faulting instruction in an infinite loop.
            libc::signal(libc::SIGSEGV, libc::SIG_DFL);
        }
    }

    pub fn install() {
        // signal-hook-registry chains to LuaJIT's prior handler automatically.
        unsafe {
            let _ = signal_hook_registry::register_unchecked(libc::SIGSEGV, handler);
        }
    }
}

#[cfg(not(unix))]
mod sigsegv {
    use std::path::Path;
    pub fn set_log_fd(_path: &Path) {}
    pub fn install() {}
}

pub fn install_panic_hook() {
    CRASH_HOOKS.get_or_init(install_crash_hooks);
}

fn install_crash_hooks() {
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        let message = if let Some(s) = panic_info.payload().downcast_ref::<&str>() {
            s.to_string()
        } else if let Some(s) = panic_info.payload().downcast_ref::<String>() {
            s.clone()
        } else {
            "Unknown panic payload".to_string()
        };

        let location = panic_info
            .location()
            .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
            .unwrap_or_else(|| "unknown location".to_string());

        tracing::error!(
            panic.message = %message,
            panic.location = %location,
            "PANIC occurred in FFF"
        );

        write_crash_report(
            "RUST PANIC",
            &format!("Message: {}\nLocation: {}", message, location),
        );
        default_panic(panic_info);
    }));

    sigsegv::install();
}

/// Parse a log level string into a `tracing::Level`.
pub fn parse_log_level(level: Option<&str>) -> tracing::Level {
    match level.as_ref().map(|s| s.trim().to_lowercase()).as_deref() {
        Some("trace") => tracing::Level::TRACE,
        Some("debug") => tracing::Level::DEBUG,
        Some("info") => tracing::Level::INFO,
        Some("warn") => tracing::Level::WARN,
        Some("error") => tracing::Level::ERROR,
        _ => tracing::Level::INFO,
    }
}

/// Default retention: how many prior nvim sessions' log files to keep.
pub const DEFAULT_RETAIN_RUNS: usize = 20;

/// Default size after which a session file rotates; the tail is kept.
pub const DEFAULT_LOG_MAX_BYTES: u64 = 32 * 1024 * 1024;

/// Settings for [`init_tracing_with`].
pub struct LogOptions<'a> {
    pub level: Option<&'a str>,
    pub retain_runs: usize,
    /// Rotate the session file once it grows past this many bytes; 0 = no cap.
    pub max_bytes: u64,
}

// Appends at a byte budget: past it the file is renamed to `<stem>.<n>.<ext>`
// and a fresh one is opened, so the writer keeps running and the newest lines
// survive. A rename another handle blocks falls back to truncating in place.
struct CappedLogFile {
    file: std::fs::File,
    path: PathBuf,
    written: u64,
    max_bytes: u64,
}

impl CappedLogFile {
    fn open(path: &Path, max_bytes: u64) -> io::Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let written = file.metadata().map(|meta| meta.len()).unwrap_or(0);
        Ok(Self {
            file,
            path: path.to_path_buf(),
            written,
            max_bytes,
        })
    }

    fn rotate(&mut self) -> io::Result<()> {
        self.file.flush()?;
        let stem = self
            .path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("fff");
        let ext = self
            .path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("log");
        let dir = self.path.parent().unwrap_or_else(|| Path::new("."));

        let mut index = 1u32;
        let rotated = loop {
            let candidate = dir.join(format!("{stem}.{index}.{ext}"));
            if !candidate.exists() {
                break candidate;
            }
            index += 1;
        };

        if std::fs::rename(&self.path, &rotated).is_ok() {
            self.file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)?;
        } else {
            self.file.set_len(0)?;
        }
        self.written = 0;
        Ok(())
    }
}

impl io::Write for CappedLogFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.max_bytes != 0
            && self.written > 0
            && self.written + buf.len() as u64 > self.max_bytes
        {
            self.rotate()?;
        }
        let written = self.file.write(buf)?;
        self.written += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.file.flush()
    }
}

pub fn generate_trace_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static TRACE_COUNTER: AtomicU64 = AtomicU64::new(0);

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);

    let pid = std::process::id() as u64;
    let counter = TRACE_COUNTER.fetch_add(1, Ordering::Relaxed);

    // very simple hash functions helps to distinguish trace ids visually
    let id = nanos ^ (pid.wrapping_mul(0x9E37_79B9_7F4A_7C15)) ^ (counter << 32);
    format!("{:016x}", id)
}

pub fn trace_span(trace_id: &str, label: &'static str) -> tracing::Span {
    tracing::info_span!("fff.trace", trace_id = trace_id, label = label)
}

fn unix_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn session_path_from_hint(hint: &Path) -> PathBuf {
    let stem = hint.file_stem().and_then(|s| s.to_str()).unwrap_or("fff");
    let ext = hint.extension().and_then(|e| e.to_str()).unwrap_or("log");
    let parent = hint.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!(
        "{stem}+{ts}+{pid}.{ext}",
        ts = unix_secs(),
        pid = std::process::id(),
    ))
}

fn rotate_logs(dir: &Path, stem: &str, ext: &str, retain_runs: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let prefix = format!("{stem}+");
    let suffix = format!(".{ext}");

    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(|res| {
            let entry = res.ok()?;
            let name = entry.file_name();
            let name = name.to_str()?;
            if !name.starts_with(&prefix) || !name.ends_with(&suffix) {
                return None;
            }

            let mtime = entry.metadata().ok()?.modified().ok()?;
            Some((mtime, entry.path()))
        })
        .collect();

    if files.len() <= retain_runs {
        return;
    }
    // Newest first, then drop everything past retain_runs.
    files.sort_by_key(|(mtime, _)| std::cmp::Reverse(*mtime));
    for (_, path) in files.into_iter().skip(retain_runs) {
        let _ = std::fs::remove_file(path);
    }
}

/// `log_file_path` is a path-shape hint. Each call writes a unique sibling
/// `<stem>+<unix-secs>+<pid>.<ext>` so concurrent processes never collide.
/// Returns the absolute path of the session file.
pub fn init_tracing(
    log_file_path: &str,
    log_level: Option<&str>,
    retain_runs: Option<usize>,
) -> Result<String, io::Error> {
    init_tracing_with(
        log_file_path,
        LogOptions {
            level: log_level,
            retain_runs: retain_runs.unwrap_or(DEFAULT_RETAIN_RUNS),
            max_bytes: DEFAULT_LOG_MAX_BYTES,
        },
    )
}

/// Same as [`init_tracing`], with an explicit byte budget per session file.
pub fn init_tracing_with(
    log_file_path: &str,
    options: LogOptions<'_>,
) -> Result<String, io::Error> {
    let hint = Path::new(log_file_path);
    let session_dir = hint
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    std::fs::create_dir_all(&session_dir)?;

    let session_path = session_path_from_hint(hint);

    // First init wins; repeat callers no-op and return the original path.
    if LOG_FILE_PATH.set(session_path.clone()).is_err() {
        return Ok(LOG_FILE_PATH
            .get()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default());
    }

    sigsegv::set_log_fd(&session_path);
    install_panic_hook();

    let stem = hint.file_stem().and_then(|s| s.to_str()).unwrap_or("fff");
    let ext = hint.extension().and_then(|e| e.to_str()).unwrap_or("log");
    rotate_logs(&session_dir, stem, ext, options.retain_runs);

    let writer_file = CappedLogFile::open(&session_path, options.max_bytes)?;

    // we intinionally leark the guard we don't ever want to stop logging
    let (non_blocking_appender, guard) = non_blocking(writer_file);
    Box::leak(Box::new(guard));

    let subscriber = tracing_subscriber::registry()
        .with(
            fmt::layer()
                .with_writer(non_blocking_appender)
                .with_target(true)
                .with_thread_ids(false)
                .with_thread_names(true)
                .with_ansi(false)
                .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE),
        )
        .with(
            EnvFilter::builder()
                .with_default_directive(parse_log_level(options.level).into())
                .from_env_lossy(),
        );

    if let Err(e) = tracing::subscriber::set_global_default(subscriber) {
        eprintln!("Failed to set tracing subscriber: {}", e);
    } else {
        tracing::info!(
            "FFF tracing initialized: {} (pid={}, retain_runs={}, max_bytes={})",
            session_path.display(),
            std::process::id(),
            options.retain_runs,
            options.max_bytes,
        );
    }

    Ok(session_path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contents(path: &Path) -> String {
        std::fs::read_to_string(path).unwrap()
    }

    #[test]
    fn a_capped_file_rotates_and_keeps_writing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fff+1+2.log");
        let mut log = CappedLogFile::open(&path, 16).unwrap();

        log.write_all(b"0123456789").unwrap();
        log.write_all(b"abcdefghij").unwrap();
        log.write_all(b"KLMN").unwrap();
        log.flush().unwrap();

        assert_eq!(contents(&dir.path().join("fff+1+2.1.log")), "0123456789");
        assert_eq!(contents(&path), "abcdefghijKLMN");
    }

    #[test]
    fn a_zero_budget_never_rotates() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("fff+3+4.log");
        let mut log = CappedLogFile::open(&path, 0).unwrap();

        log.write_all(&[b'x'; 4096]).unwrap();
        log.write_all(&[b'y'; 4096]).unwrap();
        log.flush().unwrap();

        assert_eq!(contents(&path).len(), 8192);
        assert!(!dir.path().join("fff+3+4.1.log").exists());
    }
}
