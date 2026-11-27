//! Built-in host resource monitor.
//!
//! Samples CPU, memory and disk pressure with native OS calls, turns sustained
//! breaches into warnings, and queues them for the active turn to drain. It
//! deliberately uses no subprocesses and no additional crates: the sampling
//! calls are cheap, non-blocking reads of kernel counters.
//!
//! Configuration lives in `<data_dir>/config/settings.json` under
//! `"resourceMonitor"`; `DEEPSEEK_CODE_RESOURCE_MONITOR=0|1` overrides
//! `enabled`. A missing or malformed section falls back to defaults.

use serde::Deserialize;
use std::{
    collections::{HashMap, VecDeque},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex, MutexGuard,
    },
    time::{Duration, Instant},
};

use crate::tools::background_shell::NOTIFICATION_START;

/// Task ID used in the notification envelope. It never changes, so the model
/// can recognize the source without creating a real task row.
pub const RESOURCE_MONITOR_TASK_ID: &str = "builtin-resource-monitor";
/// Environment override for `enabled`. Invalid values leave the file setting.
pub const ENABLED_ENV: &str = "DEEPSEEK_CODE_RESOURCE_MONITOR";

const MIN_INTERVAL_SECS: u64 = 2;
const MAX_INTERVAL_SECS: u64 = 300;
const MIB: u64 = 1024 * 1024;
const GIB: u64 = 1024 * MIB;
/// Recovery bands: a metric must move this far past the threshold before the
/// latched alert clears, so values flapping at the threshold do not re-alert.
const CPU_RECOVERY_MARGIN: f64 = 10.0;
const MEMORY_RECOVERY_PERCENT_MARGIN: f64 = 5.0;
const MEMORY_RECOVERY_BYTES_MARGIN: u64 = 512 * MIB;
const DISK_RECOVERY_PERCENT_MARGIN: f64 = 3.0;
const DISK_RECOVERY_BYTES_MARGIN: u64 = 512 * MIB;
/// Undrained warnings replace older ones with the same key, so an idle
/// process cannot accumulate an unbounded backlog.
const MAX_PENDING_ALERTS: usize = 8;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// User-tunable thresholds. Every field is optional in JSON.
#[derive(Clone, Debug, Deserialize)]
#[serde(default, rename_all = "camelCase")]
pub struct ResourceMonitorConfig {
    pub enabled: bool,
    pub interval_secs: u64,
    pub cpu_percent_threshold: f64,
    pub cpu_consecutive: u32,
    pub memory_available_percent_threshold: f64,
    pub memory_available_min_bytes: u64,
    pub memory_consecutive: u32,
    pub disk_available_percent_threshold: f64,
    pub disk_available_min_bytes: u64,
    pub disk_consecutive: u32,
    pub cooldown_secs: u64,
}

impl Default for ResourceMonitorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            interval_secs: 10,
            cpu_percent_threshold: 95.0,
            cpu_consecutive: 3,
            memory_available_percent_threshold: 10.0,
            memory_available_min_bytes: 2 * GIB,
            memory_consecutive: 2,
            disk_available_percent_threshold: 5.0,
            disk_available_min_bytes: 2 * GIB,
            disk_consecutive: 2,
            cooldown_secs: 300,
        }
    }
}

impl ResourceMonitorConfig {
    /// Reads `settings.json`, applies environment overrides and clamps the
    /// sampling interval. Never fails: a bad file behaves like a missing one.
    pub fn load(data_dir: &Path) -> Self {
        let setting = std::fs::read(
            crate::data_layout::DataLayout::from_root(data_dir).config_file("settings.json"),
        )
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
        .and_then(|value| value.get("resourceMonitor").cloned());
        let mut config: ResourceMonitorConfig = setting
            .and_then(|value| serde_json::from_value(value).ok())
            .unwrap_or_default();
        config.apply_environment();
        config.sanitized()
    }

    /// Keeps the interval in a range that is responsive without busy-waiting and
    /// replaces thresholds that would silently disable their comparison (`NaN`,
    /// infinities and negatives). A CPU threshold above 100 is also invalid
    /// because the sampled percent is clamped to `0.0..=100.0`, so it could
    /// never breach; memory and disk thresholds above 100 are preserved so
    /// tests can force a deterministic breach.
    pub fn sanitized(mut self) -> Self {
        let defaults = Self::default();
        self.interval_secs = self
            .interval_secs
            .clamp(MIN_INTERVAL_SECS, MAX_INTERVAL_SECS);
        if !self.cpu_percent_threshold.is_finite()
            || !(0.0..=100.0).contains(&self.cpu_percent_threshold)
        {
            self.cpu_percent_threshold = defaults.cpu_percent_threshold;
        }
        if !self.memory_available_percent_threshold.is_finite()
            || self.memory_available_percent_threshold < 0.0
        {
            self.memory_available_percent_threshold = defaults.memory_available_percent_threshold;
        }
        if !self.disk_available_percent_threshold.is_finite()
            || self.disk_available_percent_threshold < 0.0
        {
            self.disk_available_percent_threshold = defaults.disk_available_percent_threshold;
        }
        self.cpu_consecutive = self.cpu_consecutive.max(1);
        self.memory_consecutive = self.memory_consecutive.max(1);
        self.disk_consecutive = self.disk_consecutive.max(1);
        self
    }

    fn apply_environment(&mut self) {
        let Ok(value) = std::env::var(ENABLED_ENV) else {
            return;
        };
        match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" => self.enabled = true,
            "0" | "false" => self.enabled = false,
            _ => (),
        }
    }

    fn cpu_breach(&self, percent: f64) -> bool {
        percent >= self.cpu_percent_threshold
    }

    fn cpu_recovered(&self, percent: f64) -> bool {
        percent < (self.cpu_percent_threshold - CPU_RECOVERY_MARGIN).max(0.0)
    }

    /// Both conditions must hold: a large machine can have a tiny percentage
    /// free while still having plenty of GiB available, which is not a problem.
    fn memory_breach(&self, memory: &MemorySample) -> bool {
        memory.available_percent() < self.memory_available_percent_threshold
            && memory.available_bytes < self.memory_available_min_bytes
    }

    fn memory_recovered(&self, memory: &MemorySample) -> bool {
        memory.available_percent()
            >= self.memory_available_percent_threshold + MEMORY_RECOVERY_PERCENT_MARGIN
            || memory.available_bytes
                >= self
                    .memory_available_min_bytes
                    .saturating_add(MEMORY_RECOVERY_BYTES_MARGIN)
    }

    /// Either shortage is enough to run out of room.
    fn disk_breach(&self, disk: &DiskSample) -> bool {
        disk.available_percent() < self.disk_available_percent_threshold
            || disk.available_bytes < self.disk_available_min_bytes
    }

    /// Recovery needs both bands crossed, so a mount that is only barely back
    /// above one threshold does not flap the alert.
    fn disk_recovered(&self, disk: &DiskSample) -> bool {
        disk.available_percent()
            >= self.disk_available_percent_threshold + DISK_RECOVERY_PERCENT_MARGIN
            && disk.available_bytes
                >= self
                    .disk_available_min_bytes
                    .saturating_add(DISK_RECOVERY_BYTES_MARGIN)
    }
}

// ---------------------------------------------------------------------------
// Samples
// ---------------------------------------------------------------------------

/// Cumulative CPU ticks. Percent is derived from two consecutive samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuCounters {
    pub busy: u64,
    pub total: u64,
}

impl CpuCounters {
    /// `None` for the baseline sample, then the busy share of the delta.
    pub fn percent_since(&self, previous: Option<Self>) -> Option<f64> {
        let previous = previous?;
        let busy = self.busy.saturating_sub(previous.busy);
        let total = self.total.saturating_sub(previous.total);
        if total == 0 {
            return Some(0.0);
        }
        Some((busy as f64 / total as f64 * 100.0).clamp(0.0, 100.0))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemorySample {
    pub total_bytes: u64,
    pub available_bytes: u64,
}

impl MemorySample {
    pub fn available_percent(&self) -> f64 {
        percent(self.available_bytes, self.total_bytes)
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiskSample {
    /// Human-readable target names sharing this filesystem, e.g. `workspace, data`.
    pub label: String,
    pub path: PathBuf,
    pub total_bytes: u64,
    pub available_bytes: u64,
}

impl DiskSample {
    pub fn available_percent(&self) -> f64 {
        percent(self.available_bytes, self.total_bytes)
    }
}

fn percent(part: u64, total: u64) -> f64 {
    if total == 0 {
        return 0.0;
    }
    (part as f64 / total as f64 * 100.0).clamp(0.0, 100.0)
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResourceSample {
    /// `None` on the first tick, before a CPU delta exists.
    pub cpu_percent: Option<f64>,
    pub memory: Option<MemorySample>,
    pub disks: Vec<DiskSample>,
}

/// Injectable sampling backend so tests never depend on host load.
pub trait SystemSampler: Send + Sync {
    /// `disk_targets` are `(label, path)` pairs; the implementation deduplicates
    /// paths that share a filesystem and combines their labels.
    fn sample(&mut self, disk_targets: &[(String, PathBuf)]) -> ResourceSample;
}

// ---------------------------------------------------------------------------
// Native samplers
// ---------------------------------------------------------------------------

/// Reads native OS counters. Cheap enough to call inline on the async task.
#[derive(Debug, Default)]
pub struct NativeSystemSampler {
    previous_cpu: Option<CpuCounters>,
}

impl NativeSystemSampler {
    pub fn new() -> Self {
        Self::default()
    }
}

impl SystemSampler for NativeSystemSampler {
    fn sample(&mut self, disk_targets: &[(String, PathBuf)]) -> ResourceSample {
        let cpu_percent = sample_cpu_counters().and_then(|counters| {
            let percent = counters.percent_since(self.previous_cpu);
            self.previous_cpu = Some(counters);
            percent
        });
        ResourceSample {
            cpu_percent,
            memory: sample_memory(),
            disks: sample_disks(disk_targets),
        }
    }
}

// `libc` deprecates `mach_host_self` in favor of the `mach2` crate, but this
// monitor must not add dependencies. The call is stable Mach API.
#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn sample_cpu_counters() -> Option<CpuCounters> {
    use std::mem::zeroed;
    unsafe {
        let mut info: libc::host_cpu_load_info = zeroed();
        let mut count = libc::HOST_CPU_LOAD_INFO_COUNT as libc::mach_msg_type_number_t;
        let result = libc::host_statistics(
            libc::mach_host_self(),
            libc::HOST_CPU_LOAD_INFO,
            &mut info as *mut _ as libc::host_info_t,
            &mut count,
        );
        if result != libc::KERN_SUCCESS {
            return None;
        }
        let tick = |state: libc::c_int| info.cpu_ticks[state as usize] as u64;
        let busy = tick(libc::CPU_STATE_USER)
            .saturating_add(tick(libc::CPU_STATE_SYSTEM))
            .saturating_add(tick(libc::CPU_STATE_NICE));
        Some(CpuCounters {
            busy,
            total: busy.saturating_add(tick(libc::CPU_STATE_IDLE)),
        })
    }
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn sample_cpu_counters() -> Option<CpuCounters> {
    parse_proc_stat(&std::fs::read_to_string("/proc/stat").ok()?)
}

/// Parses the aggregate `cpu` line of `/proc/stat`.
#[cfg(any(target_os = "linux", target_os = "android", test))]
fn parse_proc_stat(text: &str) -> Option<CpuCounters> {
    let line = text.lines().find(|line| line.starts_with("cpu "))?;
    let values: Vec<u64> = line
        .split_whitespace()
        .skip(1)
        .filter_map(|value| value.parse().ok())
        .collect();
    if values.len() < 4 {
        return None;
    }
    let total: u64 = values.iter().copied().fold(0u64, u64::saturating_add);
    let idle = values[3].saturating_add(values.get(4).copied().unwrap_or(0));
    Some(CpuCounters {
        busy: total.saturating_sub(idle),
        total,
    })
}

#[cfg(windows)]
fn sample_cpu_counters() -> Option<CpuCounters> {
    #[repr(C)]
    #[derive(Default)]
    struct FileTime {
        low: u32,
        high: u32,
    }
    impl FileTime {
        fn value(&self) -> u64 {
            (self.high as u64) << 32 | self.low as u64
        }
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GetSystemTimes(
            idle_time: *mut FileTime,
            kernel_time: *mut FileTime,
            user_time: *mut FileTime,
        ) -> i32;
    }
    let (mut idle, mut kernel, mut user) = (
        FileTime::default(),
        FileTime::default(),
        FileTime::default(),
    );
    // SAFETY: all three pointers refer to initialized, writable structs.
    if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
        return None;
    }
    let user = user.value();
    let kernel = kernel.value();
    let idle = idle.value();
    // Kernel time includes idle time; only the non-idle part is busy.
    let busy = kernel.saturating_sub(idle).saturating_add(user);
    Some(CpuCounters {
        busy,
        total: kernel.saturating_add(user),
    })
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "android",
    windows
)))]
fn sample_cpu_counters() -> Option<CpuCounters> {
    None
}

#[cfg(target_os = "macos")]
#[allow(deprecated)]
fn sample_memory() -> Option<MemorySample> {
    use std::mem::zeroed;
    let total = macos_total_memory()?;
    unsafe {
        let mut stats: libc::vm_statistics64 = zeroed();
        let mut count = libc::HOST_VM_INFO64_COUNT;
        let result = libc::host_statistics64(
            libc::mach_host_self(),
            libc::HOST_VM_INFO64,
            &mut stats as *mut _ as libc::host_info64_t,
            &mut count,
        );
        if result != libc::KERN_SUCCESS {
            return None;
        }
        let page_size = libc::sysconf(libc::_SC_PAGESIZE);
        if page_size <= 0 {
            return None;
        }
        let page_size = page_size as u64;
        // Active, wired and compressed pages are effectively unavailable.
        let used = (stats.active_count as u64)
            .saturating_add(stats.wire_count as u64)
            .saturating_add(stats.compressor_page_count as u64)
            .saturating_mul(page_size);
        Some(MemorySample {
            total_bytes: total,
            available_bytes: total.saturating_sub(used).min(total),
        })
    }
}

#[cfg(target_os = "macos")]
fn macos_total_memory() -> Option<u64> {
    let mut value: u64 = 0;
    let mut size = std::mem::size_of::<u64>();
    let mut mib = [libc::CTL_HW, libc::HW_MEMSIZE];
    // SAFETY: mib and value are valid, correctly sized buffers.
    let result = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            mib.len() as u32,
            &mut value as *mut _ as *mut libc::c_void,
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    (result == 0 && value > 0).then_some(value)
}

#[cfg(any(target_os = "linux", target_os = "android"))]
fn sample_memory() -> Option<MemorySample> {
    parse_meminfo(&std::fs::read_to_string("/proc/meminfo").ok()?)
}

/// Parses `/proc/meminfo`; values are KiB. Falls back to free+buffers+cached
/// on kernels that predate `MemAvailable`.
#[cfg(any(target_os = "linux", target_os = "android", test))]
fn parse_meminfo(text: &str) -> Option<MemorySample> {
    fn field(text: &str, key: &str) -> Option<u64> {
        text.lines().find_map(|line| {
            let mut parts = line.split_whitespace();
            (parts.next()? == key)
                .then(|| parts.next()?.parse().ok())
                .flatten()
        })
    }
    let total_kb = field(text, "MemTotal:")?;
    let available_kb = field(text, "MemAvailable:").or_else(|| {
        let free = field(text, "MemFree:")?;
        let buffers = field(text, "Buffers:").unwrap_or(0);
        let cached = field(text, "Cached:").unwrap_or(0);
        Some(free.saturating_add(buffers).saturating_add(cached))
    })?;
    let total_bytes = total_kb.saturating_mul(1024);
    if total_bytes == 0 {
        return None;
    }
    Some(MemorySample {
        total_bytes,
        available_bytes: available_kb.saturating_mul(1024).min(total_bytes),
    })
}

#[cfg(windows)]
fn sample_memory() -> Option<MemorySample> {
    use std::mem::{size_of, zeroed};
    #[repr(C)]
    #[allow(dead_code)] // Fields required by the FFI layout, not all read here.
    struct MemoryStatusEx {
        length: u32,
        memory_load: u32,
        total_physical: u64,
        available_physical: u64,
        total_page_file: u64,
        available_page_file: u64,
        total_virtual: u64,
        available_virtual: u64,
        available_extended_virtual: u64,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn GlobalMemoryStatusEx(buffer: *mut MemoryStatusEx) -> i32;
    }
    // SAFETY: the struct is zeroed and its length field is set as required.
    let mut status: MemoryStatusEx = unsafe { zeroed() };
    status.length = size_of::<MemoryStatusEx>() as u32;
    if unsafe { GlobalMemoryStatusEx(&mut status) } == 0 || status.total_physical == 0 {
        return None;
    }
    Some(MemorySample {
        total_bytes: status.total_physical,
        available_bytes: status.available_physical.min(status.total_physical),
    })
}

#[cfg(not(any(
    target_os = "macos",
    target_os = "linux",
    target_os = "android",
    windows
)))]
fn sample_memory() -> Option<MemorySample> {
    None
}

#[cfg(any(unix, windows))]
fn sample_disks(disk_targets: &[(String, PathBuf)]) -> Vec<DiskSample> {
    group_targets(disk_targets, filesystem_identity)
        .into_iter()
        .filter_map(|(label, path)| disk_sample(label, path))
        .collect()
}

#[cfg(not(any(unix, windows)))]
fn sample_disks(_disk_targets: &[(String, PathBuf)]) -> Vec<DiskSample> {
    Vec::new()
}

/// Groups targets that resolve to the same filesystem, preserving first-seen
/// order and joining their labels. A missing target uses its nearest existing
/// ancestor, which is the filesystem that will hold the directory once created.
fn group_targets<K: PartialEq>(
    targets: &[(String, PathBuf)],
    identity: impl Fn(&Path) -> Option<K>,
) -> Vec<(String, PathBuf)> {
    let mut groups: Vec<(K, Vec<String>, PathBuf)> = Vec::new();
    for (label, path) in targets {
        let Some(existing) = nearest_existing(path) else {
            continue;
        };
        let Some(key) = identity(&existing) else {
            continue;
        };
        match groups.iter_mut().find(|group| group.0 == key) {
            Some((_, labels, _)) => {
                if !labels.contains(label) {
                    labels.push(label.clone());
                }
            }
            None => groups.push((key, vec![label.clone()], existing)),
        }
    }
    groups
        .into_iter()
        .map(|(_, labels, path)| (labels.join(", "), path))
        .collect()
}

fn nearest_existing(path: &Path) -> Option<PathBuf> {
    let mut candidate = path;
    loop {
        if candidate.exists() {
            return Some(candidate.to_path_buf());
        }
        candidate = candidate.parent()?;
    }
}

#[cfg(unix)]
fn filesystem_identity(path: &Path) -> Option<u64> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata(path).ok().map(|metadata| metadata.dev())
}

#[cfg(windows)]
fn filesystem_identity(path: &Path) -> Option<String> {
    // The volume root is stable across a path that does not exist yet.
    let canonical = std::fs::canonicalize(path).ok()?;
    let mut components = canonical.components();
    let mut root = String::new();
    for _ in 0..2 {
        root.push_str(&components.next()?.as_os_str().to_string_lossy());
    }
    Some(root.to_ascii_lowercase())
}

#[cfg(unix)]
fn disk_sample(label: String, path: PathBuf) -> Option<DiskSample> {
    use std::{ffi::CString, mem::zeroed, os::unix::ffi::OsStrExt};
    let c_path = CString::new(path.as_os_str().as_bytes()).ok()?;
    let mut stats: libc::statvfs = unsafe { zeroed() };
    // SAFETY: c_path is NUL-terminated and stats is a valid output buffer.
    if unsafe { libc::statvfs(c_path.as_ptr(), &mut stats) } != 0 {
        return None;
    }
    let block_size = if stats.f_frsize != 0 {
        stats.f_frsize as u64
    } else {
        stats.f_bsize as u64
    };
    if block_size == 0 {
        return None;
    }
    let total = (stats.f_blocks as u64).saturating_mul(block_size);
    let available = (stats.f_bavail as u64)
        .saturating_mul(block_size)
        .min(total);
    Some(DiskSample {
        label,
        path,
        total_bytes: total,
        available_bytes: available,
    })
}

#[cfg(windows)]
fn disk_sample(label: String, path: PathBuf) -> Option<DiskSample> {
    use std::os::windows::ffi::OsStrExt;
    #[link(name = "kernel32")]
    extern "system" {
        fn GetDiskFreeSpaceExW(
            directory_name: *const u16,
            free_bytes_available: *mut u64,
            total_number_of_bytes: *mut u64,
            total_number_of_free_bytes: *mut u64,
        ) -> i32;
    }
    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let (mut available, mut total, mut _total_free) = (0u64, 0u64, 0u64);
    // SAFETY: wide is NUL-terminated and the three out-pointers are writable.
    if unsafe { GetDiskFreeSpaceExW(wide.as_ptr(), &mut available, &mut total, &mut _total_free) }
        == 0
        || total == 0
    {
        return None;
    }
    Some(DiskSample {
        label,
        path,
        total_bytes: total,
        available_bytes: available.min(total),
    })
}

#[cfg(not(any(unix, windows)))]
fn disk_sample(_label: String, _path: PathBuf) -> Option<DiskSample> {
    None
}

// ---------------------------------------------------------------------------
// Alerts
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceKind {
    Cpu,
    Memory,
    Disk,
}

impl ResourceKind {
    /// Stable machine name used by the CLI and Desktop JSON events.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Memory => "memory",
            Self::Disk => "disk",
        }
    }
}

/// Identity of one monitored resource. Disk keys use the combined label so two
/// targets on the same filesystem share one alert state.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum ResourceKey {
    Cpu,
    Memory,
    Disk(String),
}

impl ResourceKey {
    fn kind(&self) -> ResourceKind {
        match self {
            Self::Cpu => ResourceKind::Cpu,
            Self::Memory => ResourceKind::Memory,
            Self::Disk(_) => ResourceKind::Disk,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ResourceAlert {
    /// Resource identity, also used to replace stale undrained alerts.
    pub key: ResourceKey,
    pub kind: ResourceKind,
    /// One-line human-readable state.
    pub summary: String,
    /// Actionable guidance for the agent.
    pub details: String,
}

impl ResourceAlert {
    fn new(key: ResourceKey, summary: String, details: String) -> Self {
        Self {
            kind: key.kind(),
            key,
            summary,
            details,
        }
    }

    fn cpu(percent: f64, config: &ResourceMonitorConfig) -> Self {
        Self::new(
            ResourceKey::Cpu,
            format!("High CPU usage: {percent:.1}% sustained"),
            cpu_details(config),
        )
    }

    fn memory(sample: &MemorySample, config: &ResourceMonitorConfig) -> Self {
        Self::new(
            ResourceKey::Memory,
            format!(
                "Low memory: {} available ({:.1}% of {})",
                format_bytes(sample.available_bytes),
                sample.available_percent(),
                format_bytes(sample.total_bytes)
            ),
            memory_details(config),
        )
    }

    fn disk(sample: &DiskSample, config: &ResourceMonitorConfig) -> Self {
        Self::new(
            ResourceKey::Disk(sample.label.clone()),
            format!(
                "Low disk space ({}): {} free ({:.1}% of {})",
                sample.label,
                format_bytes(sample.available_bytes),
                sample.available_percent(),
                format_bytes(sample.total_bytes)
            ),
            disk_details(config),
        )
    }

    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// The notification the model reads. It uses the same envelope as managed
    /// shell tasks; `<` and `>` are replaced so the summary can never break it.
    pub fn notification(&self) -> String {
        format!(
            "{NOTIFICATION_START}\n<task-id>{RESOURCE_MONITOR_TASK_ID}</task-id>\n<kind>system_resource</kind>\n<status>warning</status>\n<summary>{}</summary>\n<event>{}</event>\n</task-notification>",
            escape(&format!("System resource warning: {}", self.summary)),
            escape(&self.details)
        )
    }
}

fn escape(text: &str) -> String {
    text.chars()
        .map(|ch| match ch {
            '<' => '‹',
            '>' => '›',
            other => other,
        })
        .collect()
}

fn format_bytes(bytes: u64) -> String {
    const GIB_F: f64 = GIB as f64;
    const MIB_F: f64 = MIB as f64;
    let bytes = bytes as f64;
    if bytes >= GIB_F {
        format!("{:.1} GiB", bytes / GIB_F)
    } else {
        format!("{:.1} MiB", bytes / MIB_F)
    }
}

fn cpu_details(config: &ResourceMonitorConfig) -> String {
    format!(
        "CPU usage is at or above {:.1}% across consecutive samples. Reduce parallel work \
         (for example a lower build `-j` value), avoid starting another heavy build or test run, \
         and wait for running commands to finish before adding more.",
        config.cpu_percent_threshold
    )
}

fn memory_details(config: &ResourceMonitorConfig) -> String {
    format!(
        "Available memory is below {:.1}% and under {}. Reduce parallel jobs and the size of \
         command output, avoid loading large files into memory, and finish or stop work that is \
         no longer needed.",
        config.memory_available_percent_threshold,
        format_bytes(config.memory_available_min_bytes)
    )
}

fn disk_details(config: &ResourceMonitorConfig) -> String {
    format!(
        "Free disk space is below {:.1}% or under {}. Remove temporary build artifacts and large \
         generated files, and avoid writing further large outputs until space is available.",
        config.disk_available_percent_threshold,
        format_bytes(config.disk_available_min_bytes)
    )
}

// ---------------------------------------------------------------------------
// Evaluator
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct ResourceState {
    consecutive_breaches: u32,
    /// Latched after an alert; cleared only once the metric recovers.
    active: bool,
    last_alerted: Option<Instant>,
}

/// Pure, synchronous state machine. Time is supplied by the caller so tests
/// can drive cooldowns without sleeping.
pub struct ResourceEvaluator {
    config: ResourceMonitorConfig,
    states: HashMap<ResourceKey, ResourceState>,
}

#[derive(Debug, Default, PartialEq)]
pub struct EvaluationOutcome {
    pub alerts: Vec<ResourceAlert>,
    /// Keys that returned to their recovery band on this sample. A warning
    /// queued for one of them is stale even before its TTL expires.
    pub recovered: Vec<ResourceKey>,
    /// Fresh readings for keys that are still breaching during the cooldown.
    /// The monitor refreshes an undrained warning for them in place, so a long
    /// tool call cannot let a valid warning expire while pressure persists.
    pub breaching: Vec<ResourceAlert>,
}

/// What one monitored key did on this sample.
enum UpdateOutcome {
    None,
    Alert,
    /// Still breaching while latched and inside the cooldown.
    Ongoing,
    Recovered,
}

impl ResourceEvaluator {
    pub fn new(config: ResourceMonitorConfig) -> Self {
        Self {
            config,
            states: HashMap::new(),
        }
    }

    pub fn evaluate(&mut self, sample: &ResourceSample, now: Instant) -> Vec<ResourceAlert> {
        self.evaluate_with_recovery(sample, now).alerts
    }

    /// Like [`Self::evaluate`], but also reports every key that returned to its
    /// recovery band so the monitor can drop a warning queued while idle.
    pub fn evaluate_with_recovery(
        &mut self,
        sample: &ResourceSample,
        now: Instant,
    ) -> EvaluationOutcome {
        let mut outcome = EvaluationOutcome::default();

        if let Some(percent) = sample.cpu_percent {
            let breach = self.config.cpu_breach(percent);
            let recovered = self.config.cpu_recovered(percent);
            let required = self.config.cpu_consecutive.max(1);
            match self.update(ResourceKey::Cpu, breach, recovered, now, required) {
                UpdateOutcome::Alert => outcome
                    .alerts
                    .push(ResourceAlert::cpu(percent, &self.config)),
                UpdateOutcome::Ongoing => outcome
                    .breaching
                    .push(ResourceAlert::cpu(percent, &self.config)),
                UpdateOutcome::Recovered => outcome.recovered.push(ResourceKey::Cpu),
                UpdateOutcome::None => {}
            }
        }

        if let Some(memory) = &sample.memory {
            let breach = self.config.memory_breach(memory);
            let recovered = self.config.memory_recovered(memory);
            let required = self.config.memory_consecutive.max(1);
            match self.update(ResourceKey::Memory, breach, recovered, now, required) {
                UpdateOutcome::Alert => outcome
                    .alerts
                    .push(ResourceAlert::memory(memory, &self.config)),
                UpdateOutcome::Ongoing => outcome
                    .breaching
                    .push(ResourceAlert::memory(memory, &self.config)),
                UpdateOutcome::Recovered => outcome.recovered.push(ResourceKey::Memory),
                UpdateOutcome::None => {}
            }
        }

        for disk in &sample.disks {
            let breach = self.config.disk_breach(disk);
            let recovered = self.config.disk_recovered(disk);
            let required = self.config.disk_consecutive.max(1);
            let key = ResourceKey::Disk(disk.label.clone());
            match self.update(key, breach, recovered, now, required) {
                UpdateOutcome::Alert => {
                    outcome.alerts.push(ResourceAlert::disk(disk, &self.config))
                }
                UpdateOutcome::Ongoing => outcome
                    .breaching
                    .push(ResourceAlert::disk(disk, &self.config)),
                UpdateOutcome::Recovered => outcome
                    .recovered
                    .push(ResourceKey::Disk(disk.label.clone())),
                UpdateOutcome::None => {}
            }
        }

        outcome
    }

    fn update(
        &mut self,
        key: ResourceKey,
        breach: bool,
        recovered: bool,
        now: Instant,
        required: u32,
    ) -> UpdateOutcome {
        let cooldown = Duration::from_secs(self.config.cooldown_secs);
        let state = self.states.entry(key).or_default();

        if !breach {
            // A spike must build up again from zero.
            state.consecutive_breaches = 0;
            // Hysteresis: between threshold and recovery band the alert stays
            // latched, so flapping values do not emit a new warning.
            if recovered {
                state.active = false;
                return UpdateOutcome::Recovered;
            }
            return UpdateOutcome::None;
        }

        state.consecutive_breaches = state.consecutive_breaches.saturating_add(1);
        let due = match state.last_alerted {
            None => true,
            Some(last) => now.saturating_duration_since(last) >= cooldown,
        };

        if !state.active {
            if state.consecutive_breaches >= required && due {
                state.active = true;
                state.last_alerted = Some(now);
                return UpdateOutcome::Alert;
            }
            return UpdateOutcome::None;
        }

        // Already latched: remind periodically while the pressure lasts, but
        // never more often than the cooldown. Between reminders the warning is
        // still live, which keeps an undrained alert from expiring.
        if due {
            state.last_alerted = Some(now);
            return UpdateOutcome::Alert;
        }
        UpdateOutcome::Ongoing
    }
}

// ---------------------------------------------------------------------------
// Async handle
// ---------------------------------------------------------------------------

struct PendingAlert {
    alert: ResourceAlert,
    created_at: Instant,
}

struct Inner {
    enabled: AtomicBool,
    data_dir: PathBuf,
    workspace: Mutex<PathBuf>,
    sampler: Mutex<Box<dyn SystemSampler>>,
    evaluator: Mutex<ResourceEvaluator>,
    alerts: Mutex<VecDeque<PendingAlert>>,
    /// A warning that was not drained within this window is stale: the pressure
    /// it described may be long gone, so it must never reach a later turn.
    stale_ttl: Duration,
    latest: Mutex<Option<ResourceSample>>,
    task: Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl Drop for Inner {
    fn drop(&mut self) {
        if let Some(task) = lock(&self.task).take() {
            task.abort();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Clonable handle to the in-process monitor. Dropping the last clone stops
/// the background task; `stop()` does the same explicitly.
#[derive(Clone)]
pub struct ResourceMonitor {
    inner: Arc<Inner>,
}

impl ResourceMonitor {
    pub fn start(config: ResourceMonitorConfig, workspace: PathBuf, data_dir: PathBuf) -> Self {
        Self::start_with_sampler(
            config,
            workspace,
            data_dir,
            Box::new(NativeSystemSampler::new()),
        )
    }

    /// Testable entry point. A disabled config (or no Tokio runtime) spawns no
    /// task at all and reports `is_enabled() == false`.
    pub fn start_with_sampler(
        config: ResourceMonitorConfig,
        workspace: PathBuf,
        data_dir: PathBuf,
        sampler: Box<dyn SystemSampler>,
    ) -> Self {
        let config = config.sanitized();
        let stale_ttl = Duration::from_secs(config.interval_secs.saturating_mul(2).max(30));
        let runtime = if config.enabled {
            tokio::runtime::Handle::try_current().ok()
        } else {
            None
        };
        if config.enabled && runtime.is_none() {
            tracing::warn!(
                module = "ResourceMonitor",
                "No Tokio runtime; the resource monitor stays disabled"
            );
        }

        let inner = Arc::new(Inner {
            enabled: AtomicBool::new(runtime.is_some()),
            data_dir,
            workspace: Mutex::new(workspace),
            sampler: Mutex::new(sampler),
            evaluator: Mutex::new(ResourceEvaluator::new(config.clone())),
            alerts: Mutex::new(VecDeque::new()),
            stale_ttl,
            latest: Mutex::new(None),
            task: Mutex::new(None),
        });

        if let Some(runtime) = runtime {
            let weak = Arc::downgrade(&inner);
            let period = Duration::from_secs(config.interval_secs);
            let task = runtime.spawn(async move {
                let mut ticker = tokio::time::interval(period);
                // Under load, skip missed ticks instead of sampling in a burst.
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    ticker.tick().await;
                    let Some(inner) = weak.upgrade() else {
                        break;
                    };
                    let targets = {
                        let workspace = lock(&inner.workspace).clone();
                        vec![
                            ("workspace".to_string(), workspace),
                            ("data".to_string(), inner.data_dir.clone()),
                        ]
                    };
                    // sysctl/statvfs/proc reads are sub-millisecond, so this
                    // stays on the task instead of churning the blocking pool.
                    let sample = lock(&inner.sampler).sample(&targets);
                    let now = Instant::now();
                    let outcome = lock(&inner.evaluator).evaluate_with_recovery(&sample, now);
                    {
                        let mut pending = lock(&inner.alerts);
                        apply_outcome(&mut pending, outcome, now, inner.stale_ttl);
                    }
                    // Published after the queue update so observers of the
                    // latest sample see a fully processed tick.
                    *lock(&inner.latest) = Some(sample);
                }
            });
            *lock(&inner.task) = Some(task);
        }

        Self { inner }
    }

    /// Follows a changed working directory without restarting the monitor.
    pub fn update_workspace(&self, workspace: PathBuf) {
        *lock(&self.inner.workspace) = workspace;
    }

    /// The disk target the sampler currently reports as `workspace`.
    #[cfg(test)]
    pub fn tracked_workspace(&self) -> PathBuf {
        lock(&self.inner.workspace).clone()
    }

    /// Returns and clears pending warnings. Active turns call this; idle
    /// processes simply keep the bounded, deduplicated queue.
    pub fn drain_alerts(&self) -> Vec<ResourceAlert> {
        self.drain_alerts_at(Instant::now())
    }

    /// Like [`Self::drain_alerts`], with the caller's clock. Expired warnings
    /// are pruned first, so a turn that starts after a long idle stall can
    /// never receive a warning the monitor already knows is stale.
    pub fn drain_alerts_at(&self, now: Instant) -> Vec<ResourceAlert> {
        let mut pending = lock(&self.inner.alerts);
        let stale_ttl = self.inner.stale_ttl;
        pending.retain(|alert| now.saturating_duration_since(alert.created_at) <= stale_ttl);
        pending.drain(..).map(|pending| pending.alert).collect()
    }

    pub fn latest_sample(&self) -> Option<ResourceSample> {
        lock(&self.inner.latest).clone()
    }

    pub fn is_enabled(&self) -> bool {
        self.inner.enabled.load(Ordering::Relaxed)
    }

    /// Cancels the background task. Safe to call more than once.
    pub fn stop(&self) {
        self.inner.enabled.store(false, Ordering::Relaxed);
        if let Some(task) = lock(&self.inner.task).take() {
            task.abort();
        }
    }
}

/// Applies one evaluation to the pending queue. A key that is still breaching
/// keeps its undrained warning alive and up to date, so a long tool call cannot
/// watch a valid warning expire while pressure persists. Refreshing never
/// inserts a warning: after delivery the cooldown still throttles reminders.
fn apply_outcome(
    queue: &mut VecDeque<PendingAlert>,
    outcome: EvaluationOutcome,
    now: Instant,
    stale_ttl: Duration,
) {
    for alert in outcome.breaching {
        if let Some(existing) = queue
            .iter_mut()
            .find(|queued| queued.alert.key == alert.key)
        {
            *existing = PendingAlert {
                alert,
                created_at: now,
            };
        }
    }
    // A recovered or expired warning must be gone before a new one for the same
    // key replaces it.
    prune_alerts(queue, &outcome.recovered, now, stale_ttl);
    for alert in outcome.alerts {
        enqueue_alert(queue, alert, now);
    }
}

fn enqueue_alert(queue: &mut VecDeque<PendingAlert>, alert: ResourceAlert, created_at: Instant) {
    if let Some(existing) = queue
        .iter_mut()
        .find(|queued| queued.alert.key == alert.key)
    {
        *existing = PendingAlert { alert, created_at };
        return;
    }
    if queue.len() >= MAX_PENDING_ALERTS {
        queue.pop_front();
    }
    queue.push_back(PendingAlert { alert, created_at });
}

/// Drops warnings for keys that recovered on this sample and warnings older
/// than the stale TTL. Leaving either in the queue would inject stale pressure
/// into a turn that starts much later.
fn prune_alerts(
    queue: &mut VecDeque<PendingAlert>,
    recovered: &[ResourceKey],
    now: Instant,
    stale_ttl: Duration,
) {
    queue.retain(|pending| {
        !recovered.contains(&pending.alert.key)
            && now.saturating_duration_since(pending.created_at) <= stale_ttl
    });
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::data_layout::write_config_file;
    use crate::tools::background_shell::notification_summary;
    use std::sync::atomic::AtomicUsize;
    use tempfile::TempDir;

    static ENV: Mutex<()> = Mutex::new(());

    struct EnvGuard {
        previous: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(value: Option<&str>) -> Self {
            let previous = std::env::var_os(ENABLED_ENV);
            match value {
                Some(value) => std::env::set_var(ENABLED_ENV, value),
                None => std::env::remove_var(ENABLED_ENV),
            }
            Self { previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(ENABLED_ENV, value),
                None => std::env::remove_var(ENABLED_ENV),
            }
        }
    }

    /// Load reads the process environment, so config tests must not overlap.
    fn env_lock() -> (MutexGuard<'static, ()>, EnvGuard) {
        let guard = ENV.lock().unwrap_or_else(|error| error.into_inner());
        let env = EnvGuard::set(None);
        (guard, env)
    }

    fn temp_root() -> (TempDir, PathBuf) {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("data");
        (temp, root)
    }

    fn settings_path(root: &Path) -> PathBuf {
        crate::data_layout::DataLayout::from_root(root).config_file("settings.json")
    }

    fn default_evaluator() -> ResourceEvaluator {
        let mut config = ResourceMonitorConfig::default();
        config.cpu_consecutive = 1;
        config.memory_consecutive = 1;
        config.disk_consecutive = 1;
        config.cooldown_secs = 0;
        ResourceEvaluator::new(config)
    }

    fn cpu_sample(percent: f64) -> ResourceSample {
        ResourceSample {
            cpu_percent: Some(percent),
            ..ResourceSample::default()
        }
    }

    fn memory_sample(total_gib: u64, available_mib: u64) -> MemorySample {
        MemorySample {
            total_bytes: total_gib * GIB,
            available_bytes: available_mib * MIB,
        }
    }

    fn disk_sample(label: &str, total_gib: u64, available_mib: u64) -> DiskSample {
        DiskSample {
            label: label.to_string(),
            path: PathBuf::from("/volume"),
            total_bytes: total_gib * GIB,
            available_bytes: available_mib * MIB,
        }
    }

    // ---- configuration ---------------------------------------------------

    #[test]
    fn load_uses_defaults_when_settings_are_missing_or_invalid() {
        let (_lock, _env) = env_lock();
        let (_temp, root) = temp_root();
        let config = ResourceMonitorConfig::load(&root);
        assert!(config.enabled);
        assert_eq!(config.interval_secs, 10);
        assert_eq!(config.cpu_percent_threshold, 95.0);
        assert_eq!(config.cpu_consecutive, 3);
        assert_eq!(config.memory_available_min_bytes, 2 * GIB);
        assert_eq!(config.cooldown_secs, 300);

        write_config_file(settings_path(&root), "{not json").unwrap();
        assert_eq!(
            ResourceMonitorConfig::load(&root).interval_secs,
            config.interval_secs
        );

        write_config_file(
            settings_path(&root),
            serde_json::json!({"resourceMonitor": {"enabled": "wrong type"}}).to_string(),
        )
        .unwrap();
        assert_eq!(
            ResourceMonitorConfig::load(&root).interval_secs,
            config.interval_secs
        );
    }

    #[test]
    fn load_reads_camel_case_settings_and_partial_objects() {
        let (_lock, _env) = env_lock();
        let (_temp, root) = temp_root();
        write_config_file(
            settings_path(&root),
            serde_json::json!({
                "resourceMonitor": {
                    "enabled": false,
                    "intervalSecs": 45,
                    "cpuPercentThreshold": 80.0,
                    "memoryAvailablePercentThreshold": 4.0,
                    "memoryAvailableMinBytes": 1073741824u64,
                    "diskAvailablePercentThreshold": 2.0,
                    "diskAvailableMinBytes": 536870912u64,
                    "cooldownSecs": 60
                }
            })
            .to_string(),
        )
        .unwrap();
        let config = ResourceMonitorConfig::load(&root);
        assert!(!config.enabled);
        assert_eq!(config.interval_secs, 45);
        assert_eq!(config.cpu_percent_threshold, 80.0);
        assert_eq!(config.memory_available_percent_threshold, 4.0);
        assert_eq!(config.memory_available_min_bytes, GIB);
        assert_eq!(config.disk_available_percent_threshold, 2.0);
        assert_eq!(config.disk_available_min_bytes, 512 * MIB);
        assert_eq!(config.cooldown_secs, 60);
        // Omitted fields keep their defaults.
        assert_eq!(config.cpu_consecutive, 3);
        assert_eq!(config.memory_consecutive, 2);
        assert_eq!(config.disk_consecutive, 2);
    }

    #[test]
    fn environment_overrides_enabled_and_invalid_values_are_ignored() {
        let (_lock, _env) = env_lock();
        let (_temp, root) = temp_root();
        write_config_file(
            settings_path(&root),
            serde_json::json!({"resourceMonitor": {"enabled": false}}).to_string(),
        )
        .unwrap();

        {
            let _set = EnvGuard::set(Some("1"));
            assert!(ResourceMonitorConfig::load(&root).enabled);
        }
        {
            let _set = EnvGuard::set(Some(" TRUE "));
            assert!(ResourceMonitorConfig::load(&root).enabled);
        }
        {
            let _set = EnvGuard::set(Some("0"));
            write_config_file(
                settings_path(&root),
                serde_json::json!({"resourceMonitor": {"enabled": true}}).to_string(),
            )
            .unwrap();
            assert!(!ResourceMonitorConfig::load(&root).enabled);
        }
        {
            let _set = EnvGuard::set(Some("false"));
            assert!(!ResourceMonitorConfig::load(&root).enabled);
        }
        {
            let _set = EnvGuard::set(Some("maybe"));
            assert!(ResourceMonitorConfig::load(&root).enabled);
        }
    }

    #[test]
    fn interval_is_clamped_to_the_supported_range() {
        let (_lock, _env) = env_lock();
        let (_temp, root) = temp_root();
        for (configured, expected) in [(0, 2), (1, 2), (2, 2), (300, 300), (301, 300), (9999, 300)]
        {
            write_config_file(
                settings_path(&root),
                serde_json::json!({"resourceMonitor": {"intervalSecs": configured}}).to_string(),
            )
            .unwrap();
            assert_eq!(
                ResourceMonitorConfig::load(&root).interval_secs,
                expected,
                "configured {configured}"
            );
        }
    }

    #[test]
    fn sanitized_also_clamps_programmatic_config() {
        let mut low = ResourceMonitorConfig::default();
        low.interval_secs = 0;
        assert_eq!(low.sanitized().interval_secs, 2);
        let mut high = ResourceMonitorConfig::default();
        high.interval_secs = 600;
        assert_eq!(high.sanitized().interval_secs, 300);
    }

    #[test]
    fn sanitized_replaces_invalid_thresholds_and_zero_consecutive_counts() {
        let defaults = ResourceMonitorConfig::default();
        let config = ResourceMonitorConfig {
            cpu_percent_threshold: f64::NAN,
            memory_available_percent_threshold: -5.0,
            disk_available_percent_threshold: f64::INFINITY,
            cpu_consecutive: 0,
            memory_consecutive: 0,
            disk_consecutive: 0,
            ..ResourceMonitorConfig::default()
        }
        .sanitized();

        assert_eq!(config.cpu_percent_threshold, defaults.cpu_percent_threshold);
        assert_eq!(
            config.memory_available_percent_threshold,
            defaults.memory_available_percent_threshold
        );
        assert_eq!(
            config.disk_available_percent_threshold,
            defaults.disk_available_percent_threshold
        );
        assert_eq!(config.cpu_consecutive, 1);
        assert_eq!(config.memory_consecutive, 1);
        assert_eq!(config.disk_consecutive, 1);

        // The sampled CPU percent is clamped to 0..=100, so a higher threshold
        // could never breach and must fall back to the default.
        for invalid_cpu in [100.1, 105.0] {
            let above = ResourceMonitorConfig {
                cpu_percent_threshold: invalid_cpu,
                memory_available_percent_threshold: 100.1,
                disk_available_percent_threshold: 100.1,
                ..defaults.clone()
            }
            .sanitized();
            assert_eq!(
                above.cpu_percent_threshold, defaults.cpu_percent_threshold,
                "cpu threshold {invalid_cpu}"
            );
            // Deterministic core/CLI tests rely on these above-100 thresholds
            // surviving, because memory and disk breach on a lower bound.
            assert_eq!(above.memory_available_percent_threshold, 100.1);
            assert_eq!(above.disk_available_percent_threshold, 100.1);
        }
    }

    // ---- platform parsers ------------------------------------------------

    #[test]
    fn proc_stat_parser_extracts_busy_and_idle_deltas() {
        let counters = parse_proc_stat("cpu  100 50 25 1000 100 10 5 0\n").unwrap();
        assert_eq!(counters.busy, 190);
        assert_eq!(counters.total, 1290);
        // The aggregate line starts with "cpu " and is required.
        assert!(parse_proc_stat("cpu0 1 2 3 4\n").is_none());
    }

    #[test]
    fn meminfo_parser_uses_available_and_falls_back_to_free_buffers_cached() {
        let with_available = parse_meminfo(
            "MemTotal:       16384000 kB\nMemFree:         1000000 kB\nMemAvailable:    8192000 kB\n",
        )
        .unwrap();
        assert_eq!(with_available.total_bytes, 16_384_000 * 1024);
        assert_eq!(with_available.available_bytes, 8_192_000 * 1024);

        let fallback = parse_meminfo(
            "MemTotal:       16384000 kB\nMemFree:         1000000 kB\nBuffers:          200000 kB\nCached:          3000000 kB\n",
        )
        .unwrap();
        assert_eq!(
            fallback.available_bytes,
            (1_000_000 + 200_000 + 3_000_000) * 1024
        );

        assert!(parse_meminfo("Nothing: here\n").is_none());
    }

    #[test]
    fn cpu_percent_is_derived_from_deltas_and_clamped() {
        let counters = CpuCounters {
            busy: 150,
            total: 200,
        };
        assert_eq!(counters.percent_since(None), None);
        assert_eq!(
            counters.percent_since(Some(CpuCounters {
                busy: 50,
                total: 100
            })),
            Some(100.0)
        );
        assert_eq!(
            counters.percent_since(Some(CpuCounters {
                busy: 100,
                total: 100
            })),
            Some(50.0)
        );
        // A stale or identical counter sample is a zero percent, not a NaN.
        assert_eq!(
            counters.percent_since(Some(CpuCounters {
                busy: 150,
                total: 200
            })),
            Some(0.0)
        );
    }

    #[test]
    fn percentages_handle_zero_totals_and_clamp() {
        assert_eq!(
            MemorySample {
                total_bytes: 0,
                available_bytes: 0
            }
            .available_percent(),
            0.0
        );
        assert_eq!(
            MemorySample {
                total_bytes: 100,
                available_bytes: 200
            }
            .available_percent(),
            100.0
        );
    }

    // ---- native sampling -------------------------------------------------

    #[test]
    fn native_sampler_reports_real_values_and_deduplicates_mounts() {
        let temp = tempfile::tempdir().unwrap();
        let workspace = temp.path().join("workspace");
        let data = temp.path().join("data");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&data).unwrap();

        let mut sampler = NativeSystemSampler::new();
        let targets = [
            ("workspace".to_string(), workspace.clone()),
            ("data".to_string(), data.clone()),
        ];
        let first = sampler.sample(&targets);
        assert!(
            first.cpu_percent.is_none(),
            "the first CPU sample is only a baseline"
        );
        assert!(first.memory.is_some(), "native memory sampling works");
        std::thread::sleep(Duration::from_millis(50));
        let second = sampler.sample(&targets);

        let cpu = second.cpu_percent.expect("second sample has a delta");
        assert!(
            (0.0..=100.0).contains(&cpu),
            "cpu percent out of range: {cpu}"
        );

        let memory = second.memory.expect("memory is sampled");
        assert!(memory.total_bytes > 0, "physical memory is non-zero");
        assert!(memory.available_bytes <= memory.total_bytes);
        assert!((0.0..=100.0).contains(&memory.available_percent()));

        assert_eq!(
            second.disks.len(),
            1,
            "workspace and data share one filesystem here"
        );
        let disk = &second.disks[0];
        assert_eq!(disk.label, "workspace, data");
        assert!(disk.total_bytes > 0);
        assert!(disk.available_bytes <= disk.total_bytes);
        assert!((0.0..=100.0).contains(&disk.available_percent()));
    }

    #[test]
    fn group_targets_joins_labels_per_identity_and_skips_unknown_paths() {
        let temp = tempfile::tempdir().unwrap();
        let one = temp.path().join("one");
        let two = temp.path().join("two");
        let workspace = one.join("workspace");
        let data = one.join("data");
        let other = two.join("other");
        for path in [&workspace, &data, &other] {
            std::fs::create_dir_all(path).unwrap();
        }
        let targets = vec![
            ("workspace".to_string(), workspace.clone()),
            ("data".to_string(), data),
            ("other".to_string(), other.clone()),
            // Resolves to `temp`, which the fake identity does not recognize.
            ("missing".to_string(), temp.path().join("nowhere/missing")),
        ];
        let grouped = group_targets(&targets, |path| {
            if path.starts_with(&one) {
                Some(1)
            } else if path.starts_with(&two) {
                Some(2)
            } else {
                // Simulates a filesystem that cannot be inspected.
                None
            }
        });
        assert_eq!(
            grouped,
            vec![
                ("workspace, data".to_string(), workspace),
                ("other".to_string(), other),
            ]
        );
    }

    #[test]
    fn nearest_existing_walks_up_to_an_existing_ancestor() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("a/b/c");
        assert_eq!(nearest_existing(&missing), Some(temp.path().to_path_buf()));
        assert_eq!(
            nearest_existing(temp.path()),
            Some(temp.path().to_path_buf())
        );
    }

    // ---- thresholds ------------------------------------------------------

    #[test]
    fn memory_requires_both_a_low_percent_and_absolute_shortage() {
        let config = ResourceMonitorConfig::default();
        // 5% of 100 GiB is below 10%, but 5 GiB is plenty.
        let plenty = memory_sample(100, 5 * 1024);
        assert!(!config.memory_breach(&plenty));
        assert!(config.memory_recovered(&plenty));

        let shortage = memory_sample(100, 1024); // 1 GiB, 1%
        assert!(config.memory_breach(&shortage));

        // Recovery needs one of the two bands to be crossed.
        assert!(!config.memory_recovered(&memory_sample(100, 2499))); // < 2.5 GiB, 2.4%
        assert!(config.memory_recovered(&memory_sample(100, 2560))); // 2.5 GiB exactly
                                                                     // 2460 of 16384 MiB is just over 15%, even though bytes stay under 2.5 GiB.
        assert!(config.memory_recovered(&memory_sample(16, 2460)));
    }

    #[test]
    fn disk_breaches_on_either_shortage_and_recovers_on_both() {
        let config = ResourceMonitorConfig::default();
        // 4% of 100 GiB: percentage is low even though bytes are fine.
        assert!(config.disk_breach(&disk_sample("workspace", 100, 4 * 1024)));
        // 50% of 3 GiB: bytes are low even though the percentage is fine.
        assert!(config.disk_breach(&disk_sample("workspace", 3, 1536)));
        assert!(!config.disk_breach(&disk_sample("workspace", 100, 6 * 1024)));
        assert!(!config.disk_recovered(&disk_sample("workspace", 100, 7 * 1024)));
        assert!(!config.disk_recovered(&disk_sample("workspace", 100, 2 * 1024 + 256)));
        assert!(config.disk_recovered(&disk_sample("workspace", 100, 8 * 1024)));
        assert!(config.disk_recovered(&disk_sample("workspace", 16, 3072)));
    }

    // ---- evaluator state machine ----------------------------------------

    #[test]
    fn alert_waits_for_the_required_consecutive_breaches() {
        let mut config = ResourceMonitorConfig::default();
        config.cpu_consecutive = 3;
        config.cooldown_secs = 60;
        let mut evaluator = ResourceEvaluator::new(config);
        let start = Instant::now();

        assert!(evaluator.evaluate(&cpu_sample(99.0), start).is_empty());
        assert!(evaluator
            .evaluate(&cpu_sample(99.0), start + Duration::from_secs(10))
            .is_empty());
        let alerts = evaluator.evaluate(&cpu_sample(99.0), start + Duration::from_secs(20));
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].kind, ResourceKind::Cpu);
        assert_eq!(alerts[0].summary(), "High CPU usage: 99.0% sustained");
        assert!(alerts[0].details.contains("-j"));
    }

    #[test]
    fn a_spike_resets_the_consecutive_counter() {
        let mut config = ResourceMonitorConfig::default();
        config.cpu_consecutive = 3;
        config.cooldown_secs = 300;
        let mut evaluator = ResourceEvaluator::new(config);
        let start = Instant::now();
        let tick = Duration::from_secs(10);

        assert!(evaluator.evaluate(&cpu_sample(99.0), start).is_empty());
        assert!(evaluator
            .evaluate(&cpu_sample(99.0), start + tick)
            .is_empty());
        // Back in the normal band: the streak restarts even though it was one
        // sample away from alerting.
        assert!(evaluator
            .evaluate(&cpu_sample(20.0), start + tick * 2)
            .is_empty());
        assert!(evaluator
            .evaluate(&cpu_sample(99.0), start + tick * 3)
            .is_empty());
        assert!(evaluator
            .evaluate(&cpu_sample(99.0), start + tick * 4)
            .is_empty());
        assert_eq!(
            evaluator
                .evaluate(&cpu_sample(99.0), start + tick * 5)
                .len(),
            1
        );
    }

    #[test]
    fn hysteresis_keeps_the_alert_latched_between_threshold_and_recovery() {
        let mut config = ResourceMonitorConfig::default();
        config.cpu_consecutive = 3;
        config.cooldown_secs = 300;
        let start = Instant::now();

        // Latched evaluator: never crosses the 85% recovery band.
        let mut latched = ResourceEvaluator::new(config.clone());
        for second in [0, 10, 20] {
            let _ = latched.evaluate(&cpu_sample(99.0), start + Duration::from_secs(second));
        }
        assert!(latched
            .evaluate(&cpu_sample(90.0), start + Duration::from_secs(30))
            .is_empty());
        // Still latched, so the next breach reminds immediately once the
        // cooldown has passed instead of rebuilding the streak.
        let alerts = latched.evaluate(&cpu_sample(99.0), start + Duration::from_secs(400));
        assert_eq!(
            alerts.len(),
            1,
            "latched alert should re-fire after cooldown"
        );

        // Recovered evaluator: 80% crosses the band and clears the latch, so
        // the next breach must build three consecutive samples again.
        let mut recovered = ResourceEvaluator::new(config);
        for second in [0, 10, 20] {
            let _ = recovered.evaluate(&cpu_sample(99.0), start + Duration::from_secs(second));
        }
        assert!(recovered
            .evaluate(&cpu_sample(80.0), start + Duration::from_secs(30))
            .is_empty());
        assert!(recovered
            .evaluate(&cpu_sample(99.0), start + Duration::from_secs(400))
            .is_empty());
        assert!(recovered
            .evaluate(&cpu_sample(99.0), start + Duration::from_secs(410))
            .is_empty());
        assert_eq!(
            recovered
                .evaluate(&cpu_sample(99.0), start + Duration::from_secs(420))
                .len(),
            1
        );
    }

    #[test]
    fn cooldown_throttles_repeated_alerts_for_the_same_key() {
        let mut config = ResourceMonitorConfig::default();
        config.cpu_consecutive = 1;
        config.cooldown_secs = 300;
        let mut evaluator = ResourceEvaluator::new(config);
        let start = Instant::now();

        assert_eq!(evaluator.evaluate(&cpu_sample(99.0), start).len(), 1);
        assert!(evaluator
            .evaluate(&cpu_sample(99.0), start + Duration::from_secs(299))
            .is_empty());
        assert_eq!(
            evaluator
                .evaluate(&cpu_sample(99.0), start + Duration::from_secs(300))
                .len(),
            1
        );
        assert!(evaluator
            .evaluate(&cpu_sample(99.0), start + Duration::from_secs(599))
            .is_empty());
        assert_eq!(
            evaluator
                .evaluate(&cpu_sample(99.0), start + Duration::from_secs(600))
                .len(),
            1
        );
    }

    #[test]
    fn each_resource_key_alerts_independently() {
        let mut evaluator = default_evaluator();
        let now = Instant::now();
        let sample = ResourceSample {
            cpu_percent: Some(99.0),
            memory: Some(memory_sample(16, 512)),
            disks: vec![
                disk_sample("workspace", 100, 1024),
                disk_sample("data", 100, 128 * 1024),
            ],
        };
        let alerts = evaluator.evaluate(&sample, now);
        assert_eq!(alerts.len(), 3);
        assert_eq!(
            alerts.iter().map(|alert| alert.kind).collect::<Vec<_>>(),
            vec![ResourceKind::Cpu, ResourceKind::Memory, ResourceKind::Disk]
        );
        assert!(alerts[1]
            .summary()
            .starts_with("Low memory: 512.0 MiB available"));
        assert!(alerts[2]
            .summary()
            .starts_with("Low disk space (workspace): 1.0 GiB free"));
    }

    // ---- notifications ---------------------------------------------------

    #[test]
    fn notification_is_well_formed_and_escapes_angle_brackets() {
        let alert = ResourceAlert::new(
            ResourceKey::Memory,
            "Low memory: <1 GiB> available".to_string(),
            "Use fewer <heavy> jobs.".to_string(),
        );
        let text = alert.notification();
        assert!(text.starts_with(NOTIFICATION_START));
        assert!(text.ends_with("</task-notification>"));
        assert!(text.contains("<task-id>builtin-resource-monitor</task-id>"));
        assert!(text.contains("<kind>system_resource</kind>"));
        assert!(text.contains("<status>warning</status>"));
        assert!(!text.contains("<1 GiB>"));
        assert!(!text.contains("<heavy>"));
        assert!(text.contains("‹1 GiB›"));
        assert!(text.contains("‹heavy›"));

        let summary = notification_summary(&text).expect("summary is parseable");
        assert_eq!(
            summary,
            "System resource warning: Low memory: ‹1 GiB› available"
        );
        assert!(!summary.contains('<') && !summary.contains('>'));
        assert_eq!(alert.summary(), "Low memory: <1 GiB> available");
    }

    #[test]
    fn summaries_use_human_readable_units() {
        assert_eq!(format_bytes(512 * MIB), "512.0 MiB");
        assert_eq!(format_bytes(1536 * MIB), "1.5 GiB");
        assert_eq!(format_bytes(460 * GIB + 400 * MIB), "460.4 GiB");
    }

    #[test]
    fn resource_kind_names_are_stable_for_host_events() {
        assert_eq!(ResourceKind::Cpu.as_str(), "cpu");
        assert_eq!(ResourceKind::Memory.as_str(), "memory");
        assert_eq!(ResourceKind::Disk.as_str(), "disk");
    }

    // ---- async lifecycle --------------------------------------------------

    #[derive(Clone)]
    struct FakeSampler {
        sample: ResourceSample,
        calls: Arc<AtomicUsize>,
        targets: Arc<Mutex<Vec<(String, PathBuf)>>>,
    }

    impl FakeSampler {
        fn new(
            sample: ResourceSample,
        ) -> (Self, Arc<AtomicUsize>, Arc<Mutex<Vec<(String, PathBuf)>>>) {
            let calls = Arc::new(AtomicUsize::new(0));
            let targets = Arc::new(Mutex::new(Vec::new()));
            (
                Self {
                    sample,
                    calls: calls.clone(),
                    targets: targets.clone(),
                },
                calls,
                targets,
            )
        }
    }

    impl SystemSampler for FakeSampler {
        fn sample(&mut self, disk_targets: &[(String, PathBuf)]) -> ResourceSample {
            self.calls.fetch_add(1, Ordering::SeqCst);
            *self.targets.lock().unwrap() = disk_targets.to_vec();
            self.sample.clone()
        }
    }

    /// Returns each sample in order and repeats the last one afterwards.
    struct SequenceSampler {
        samples: Vec<ResourceSample>,
        calls: Arc<AtomicUsize>,
    }

    impl SequenceSampler {
        fn new(samples: Vec<ResourceSample>) -> (Self, Arc<AtomicUsize>) {
            let calls = Arc::new(AtomicUsize::new(0));
            (
                Self {
                    samples,
                    calls: calls.clone(),
                },
                calls,
            )
        }
    }

    impl SystemSampler for SequenceSampler {
        fn sample(&mut self, _disk_targets: &[(String, PathBuf)]) -> ResourceSample {
            let index = self.calls.fetch_add(1, Ordering::SeqCst);
            self.samples
                .get(index)
                .or_else(|| self.samples.last())
                .cloned()
                .unwrap_or_default()
        }
    }

    fn fast_config() -> ResourceMonitorConfig {
        let mut config = ResourceMonitorConfig::default();
        config.interval_secs = 2;
        config.cpu_consecutive = 1;
        config.cooldown_secs = 0;
        config
    }

    async fn wait_for_calls(calls: &AtomicUsize, expected: usize) {
        for _ in 0..50 {
            if calls.load(Ordering::SeqCst) >= expected {
                return;
            }
            tokio::time::advance(Duration::from_secs(2)).await;
            tokio::task::yield_now().await;
        }
        panic!(
            "sampler was called {} times, expected at least {expected}",
            calls.load(Ordering::SeqCst)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn disabled_monitor_spawns_nothing() {
        let mut config = fast_config();
        config.enabled = false;
        let (sampler, calls, _targets) = FakeSampler::new(cpu_sample(99.0));
        let monitor = ResourceMonitor::start_with_sampler(
            config,
            PathBuf::from("/workspace"),
            PathBuf::from("/data"),
            Box::new(sampler),
        );
        assert!(!monitor.is_enabled());
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert!(monitor.drain_alerts().is_empty());
        assert!(monitor.latest_sample().is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn monitor_samples_drains_deduplicates_updates_workspace_and_stops() {
        let (sampler, calls, targets) = FakeSampler::new(cpu_sample(99.0));
        let monitor = ResourceMonitor::start_with_sampler(
            fast_config(),
            PathBuf::from("/old-workspace"),
            PathBuf::from("/data"),
            Box::new(sampler),
        );
        assert!(monitor.is_enabled());

        wait_for_calls(&calls, 1).await;
        assert!(monitor.latest_sample().is_some());
        assert_eq!(
            *targets.lock().unwrap(),
            vec![
                ("workspace".to_string(), PathBuf::from("/old-workspace")),
                ("data".to_string(), PathBuf::from("/data")),
            ]
        );

        // Every tick after the first emits because the cooldown is zero, but
        // undrained alerts with the same key collapse into the newest one.
        wait_for_calls(&calls, 4).await;
        let alerts = monitor.drain_alerts();
        assert_eq!(alerts.len(), 1, "same-key alerts must be deduplicated");
        assert_eq!(alerts[0].kind, ResourceKind::Cpu);
        assert!(monitor.drain_alerts().is_empty(), "drain clears the queue");

        monitor.update_workspace(PathBuf::from("/new-workspace"));
        let before = calls.load(Ordering::SeqCst);
        wait_for_calls(&calls, before + 1).await;
        assert_eq!(
            targets.lock().unwrap()[0],
            ("workspace".to_string(), PathBuf::from("/new-workspace"))
        );

        let before_stop = calls.load(Ordering::SeqCst);
        monitor.stop();
        assert!(!monitor.is_enabled());
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            calls.load(Ordering::SeqCst),
            before_stop,
            "stop must cancel the background task"
        );
        monitor.stop();
    }

    #[tokio::test(start_paused = true)]
    async fn dropping_the_handle_stops_the_task() {
        let (sampler, calls, _targets) = FakeSampler::new(cpu_sample(99.0));
        let monitor = ResourceMonitor::start_with_sampler(
            fast_config(),
            PathBuf::from("/workspace"),
            PathBuf::from("/data"),
            Box::new(sampler),
        );
        wait_for_calls(&calls, 2).await;
        drop(monitor);
        let before = calls.load(Ordering::SeqCst);
        tokio::time::advance(Duration::from_secs(10)).await;
        tokio::task::yield_now().await;
        assert_eq!(calls.load(Ordering::SeqCst), before);
    }

    #[test]
    fn start_without_a_runtime_stays_disabled_instead_of_panicking() {
        // `Handle::try_current` is thread-local, so a plain test has none.
        let monitor = ResourceMonitor::start(
            fast_config(),
            PathBuf::from("/workspace"),
            PathBuf::from("/data"),
        );
        assert!(!monitor.is_enabled());
        assert!(monitor.drain_alerts().is_empty());
    }

    #[tokio::test(start_paused = true)]
    async fn recovery_clears_undrained_alert_for_that_key_only() {
        let mut config = fast_config();
        config.memory_consecutive = 1;
        let samples = vec![
            ResourceSample {
                cpu_percent: Some(99.0),
                memory: Some(memory_sample(16, 512)),
                ..ResourceSample::default()
            },
            ResourceSample {
                cpu_percent: Some(5.0),
                memory: Some(memory_sample(16, 512)),
                ..ResourceSample::default()
            },
        ];
        let (sampler, calls) = SequenceSampler::new(samples);
        let monitor = ResourceMonitor::start_with_sampler(
            config,
            PathBuf::from("/workspace"),
            PathBuf::from("/data"),
            Box::new(sampler),
        );

        // Tick 1 breaches both keys while nothing is draining. Tick 2 recovers
        // CPU while memory stays low, so only the CPU warning must survive
        // removal.
        wait_for_calls(&calls, 2).await;
        let alerts = monitor.drain_alerts();
        assert_eq!(alerts.len(), 1, "only the still-breaching key is delivered");
        assert_eq!(alerts[0].key, ResourceKey::Memory);
        assert_eq!(alerts[0].kind, ResourceKind::Memory);
        monitor.stop();
    }

    #[test]
    fn stale_undrained_alerts_expire_after_ttl() {
        let mut config = ResourceMonitorConfig::default();
        config.enabled = false;
        let monitor =
            ResourceMonitor::start(config, PathBuf::from("/workspace"), PathBuf::from("/data"));
        let ttl = monitor.inner.stale_ttl;
        let now = Instant::now();
        {
            let mut pending = lock(&monitor.inner.alerts);
            pending.push_back(PendingAlert {
                alert: ResourceAlert::new(
                    ResourceKey::Cpu,
                    "stale".to_string(),
                    "details".to_string(),
                ),
                created_at: now - ttl - Duration::from_secs(1),
            });
            pending.push_back(PendingAlert {
                alert: ResourceAlert::new(
                    ResourceKey::Memory,
                    "fresh".to_string(),
                    "details".to_string(),
                ),
                created_at: now,
            });
        }

        let drained = monitor.drain_alerts_at(now);
        assert_eq!(drained.len(), 1, "an alert older than the TTL is discarded");
        assert_eq!(drained[0].key, ResourceKey::Memory);
        assert!(monitor.drain_alerts_at(now).is_empty());
    }

    #[test]
    fn sustained_breach_refreshes_undrained_alert_past_stale_ttl() {
        let mut config = ResourceMonitorConfig::default();
        config.cpu_consecutive = 1;
        config.cooldown_secs = 300;
        let mut evaluator = ResourceEvaluator::new(config);
        let mut queue = VecDeque::new();
        let stale_ttl = Duration::from_secs(30);
        let start = Instant::now();

        let outcome = evaluator.evaluate_with_recovery(&cpu_sample(99.0), start);
        assert_eq!(outcome.alerts.len(), 1);
        apply_outcome(&mut queue, outcome, start, stale_ttl);
        assert_eq!(queue.len(), 1);

        // More than `stale_ttl` later the metric is still critically low, but
        // the cooldown suppresses a new alert. The undrained warning must
        // survive and carry the latest reading.
        let later = start + stale_ttl + Duration::from_secs(10);
        let outcome = evaluator.evaluate_with_recovery(&cpu_sample(97.5), later);
        assert!(
            outcome.alerts.is_empty(),
            "a cooldown reminder must not fire yet"
        );
        assert_eq!(outcome.breaching.len(), 1);
        apply_outcome(&mut queue, outcome, later, stale_ttl);
        assert_eq!(queue.len(), 1, "the still-breaching warning is not evicted");
        let delivered: Vec<ResourceAlert> = queue.drain(..).map(|pending| pending.alert).collect();
        assert_eq!(delivered.len(), 1);
        assert_eq!(delivered[0].summary(), "High CPU usage: 97.5% sustained");

        // Delivering the warning does not re-arm it: the cooldown still applies,
        // so a later still-breaching sample must not enqueue a second warning.
        let afterwards = later + Duration::from_secs(1);
        let outcome = evaluator.evaluate_with_recovery(&cpu_sample(98.0), afterwards);
        assert!(outcome.alerts.is_empty());
        assert_eq!(outcome.breaching.len(), 1);
        apply_outcome(&mut queue, outcome, afterwards, stale_ttl);
        assert!(
            queue.is_empty(),
            "a delivered alert must not be re-enqueued inside the cooldown"
        );
    }

    #[test]
    fn enqueue_replaces_same_key_and_stays_bounded() {
        let mut queue = VecDeque::new();
        let now = Instant::now();
        for index in 0..(MAX_PENDING_ALERTS + 3) {
            enqueue_alert(
                &mut queue,
                ResourceAlert::new(
                    ResourceKey::Disk(format!("disk-{index}")),
                    format!("summary {index}"),
                    "details".to_string(),
                ),
                now,
            );
        }
        assert_eq!(queue.len(), MAX_PENDING_ALERTS);
        assert_eq!(
            queue.front().unwrap().alert.key,
            ResourceKey::Disk("disk-3".to_string())
        );

        enqueue_alert(
            &mut queue,
            ResourceAlert::new(
                ResourceKey::Disk("disk-9".to_string()),
                "newer".to_string(),
                "details".to_string(),
            ),
            now + Duration::from_secs(1),
        );
        assert_eq!(queue.len(), MAX_PENDING_ALERTS);
        let replaced = queue
            .iter()
            .find(|queued| queued.alert.key == ResourceKey::Disk("disk-9".to_string()))
            .unwrap();
        assert_eq!(replaced.alert.summary, "newer");
        assert_eq!(replaced.created_at, now + Duration::from_secs(1));
    }
}
