/// Process memory in bytes. The one seam the service lets a test replace.
pub trait MemoryProbe: Send + Sync {
    /// Private resident bytes, or `None` when the OS counter is unavailable.
    fn used_bytes(&self) -> Option<u64>;
}

/// Real adapter: Windows `PrivateUsage`, Linux `VmRSS`.
pub struct OsMemoryProbe;

impl MemoryProbe for OsMemoryProbe {
    fn used_bytes(&self) -> Option<u64> {
        platform_used_bytes()
    }
}

/// Return freed pages to the allocator after an eviction.
pub(crate) fn collect_allocator() {
    // SAFETY: mi_collect only reclaims mimalloc pages of the calling heap.
    unsafe { libmimalloc_sys::mi_collect(true) };
}

#[cfg(windows)]
fn platform_used_bytes() -> Option<u64> {
    use windows_sys::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS, PROCESS_MEMORY_COUNTERS_EX,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;

    let mut counters = PROCESS_MEMORY_COUNTERS_EX::default();
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    // The EX struct starts with the plain counters; claiming the longer layout
    // in `cb` is what makes the kernel fill `PrivateUsage`.
    let ok = unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters as *mut PROCESS_MEMORY_COUNTERS_EX as *mut PROCESS_MEMORY_COUNTERS,
            size,
        )
    };
    (ok != 0).then_some(counters.PrivateUsage as u64)
}

#[cfg(target_os = "linux")]
fn platform_used_bytes() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    let line = status.lines().find(|line| line.starts_with("VmRSS:"))?;
    let kb: u64 = line
        .strip_prefix("VmRSS:")?
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kb * 1024)
}

#[cfg(not(any(windows, target_os = "linux")))]
fn platform_used_bytes() -> Option<u64> {
    None
}
