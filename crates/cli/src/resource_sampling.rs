//! Bounded local diagnostics. No prompts, commands, paths or environment values.
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
    sync::{mpsc, RwLock},
    time::{Duration, Instant},
};

static SESSION: RwLock<Option<String>> = RwLock::new(None);
const FILE_LIMIT: u64 = 1024 * 1024;
const TOTAL_LIMIT: u64 = 32 * FILE_LIMIT;
const INTERVAL: Duration = Duration::from_secs(10);

pub fn session(id: &str) {
    *SESSION.write().unwrap_or_else(|error| error.into_inner()) = Some(id.into());
}

pub struct Sampler(mpsc::Sender<()>);
impl Drop for Sampler {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

pub fn start(root: &Path) -> Option<Sampler> {
    if !cfg!(windows) || std::env::var("DCL_RESOURCE_SAMPLING").as_deref() == Ok("0") {
        return None;
    }
    let directory = root.join("logs").join("resources");
    std::fs::create_dir_all(&directory).ok()?;
    let file = directory.join(format!(
        "process-{}-{}.jsonl",
        std::process::id(),
        chrono::Utc::now().timestamp_millis()
    ));
    let (stop, receiver) = mpsc::channel();
    std::thread::Builder::new().name("resource-sampler".into()).spawn(move || {
        let mut previous = HashMap::new();
        let mut at = Instant::now();
        let mut round = 0;
        loop {
            let now = Instant::now();
            let elapsed = now.duration_since(at).as_secs_f64();
            let started = Instant::now();
            let (processes, memory) = sample();
            let mut next = HashMap::new();
            let processes = processes.into_iter().map(|process| {
                let key = (process.pid, process.created);
                let cpu = previous.get(&key).map(|old| cpu_percent(process.cpu_100ns, *old, elapsed));
                next.insert(key, process.cpu_100ns);
                serde_json::json!({"pid":process.pid,"parent_pid":process.parent,"created":process.created,
                    "cpu_percent":cpu,"rss_bytes":process.rss,"private_bytes":process.private,"threads":process.threads})
            }).collect::<Vec<_>>();
            let row = serde_json::json!({"time":chrono::Utc::now().to_rfc3339(),"pid":std::process::id(),
                "session":SESSION.read().unwrap_or_else(|error| error.into_inner()).clone(),
                "elapsed_seconds":elapsed,"logical_cpus":std::thread::available_parallelism().map_or(1,usize::from),
                "system_memory":memory,"processes":processes,"sample_ms":started.elapsed().as_millis()});
            if let Ok(mut bytes) = serde_json::to_vec(&row) { bytes.push(b'\n'); let _ = append(&file, &bytes, FILE_LIMIT); }
            if round % 6 == 0 { prune(&directory, &file); }
            round += 1;
            previous = next;
            at = now;
            if receiver.recv_timeout(INTERVAL) != Err(mpsc::RecvTimeoutError::Timeout) { break; }
        }
    }).ok()?;
    Some(Sampler(stop))
}

fn cpu_percent(current: u64, previous: u64, seconds: f64) -> f64 {
    if seconds <= 0.0 {
        return 0.0;
    }
    ((current.saturating_sub(previous) as f64 / 100_000.0 / seconds) * 100.0).round() / 100.0
}

fn append(path: &Path, bytes: &[u8], limit: u64) -> std::io::Result<()> {
    if bytes.len() as u64 > limit {
        return Ok(());
    }
    if path.metadata().map_or(0, |metadata| metadata.len()) + bytes.len() as u64 > limit {
        let previous = path.with_extension("previous.jsonl");
        if previous.exists() {
            std::fs::remove_file(&previous)?;
        }
        std::fs::rename(path, previous)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(bytes)
}

fn prune(directory: &Path, current: &Path) {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    let mut files: Vec<(PathBuf, std::fs::Metadata)> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_name().to_string_lossy().starts_with("process-")
                && entry.path().extension().is_some_and(|ext| ext == "jsonl")
        })
        .filter_map(|entry| {
            entry
                .metadata()
                .ok()
                .filter(|metadata| metadata.is_file())
                .map(|metadata| (entry.path(), metadata))
        })
        .collect();
    files.sort_by_key(|(_, metadata)| metadata.modified().ok());
    let mut total: u64 = files.iter().map(|(_, metadata)| metadata.len()).sum();
    for (path, metadata) in files {
        if total <= TOTAL_LIMIT {
            break;
        }
        if path != current && std::fs::remove_file(&path).is_ok() {
            total = total.saturating_sub(metadata.len());
        }
    }
}

struct Process {
    pid: u32,
    parent: u32,
    created: u64,
    cpu_100ns: u64,
    rss: usize,
    private: usize,
    threads: u32,
}

#[cfg(not(windows))]
fn sample() -> (Vec<Process>, serde_json::Value) {
    (Vec::new(), serde_json::Value::Null)
}

#[cfg(windows)]
fn sample() -> (Vec<Process>, serde_json::Value) {
    use std::{
        ffi::c_void,
        mem::{size_of, zeroed},
    };
    type Handle = *mut c_void;
    #[repr(C)]
    struct Entry {
        size: u32,
        usage: u32,
        pid: u32,
        heap: usize,
        module: u32,
        threads: u32,
        parent: u32,
        priority: i32,
        flags: u32,
        name: [u16; 260],
    }
    #[repr(C)]
    #[derive(Default)]
    struct Time {
        low: u32,
        high: u32,
    }
    impl Time {
        fn value(&self) -> u64 {
            (self.high as u64) << 32 | self.low as u64
        }
    }
    #[repr(C)]
    struct Memory {
        size: u32,
        faults: u32,
        peak: usize,
        rss: usize,
        peak_paged: usize,
        paged: usize,
        peak_nonpaged: usize,
        nonpaged: usize,
        pagefile: usize,
        peak_pagefile: usize,
        private: usize,
    }
    #[repr(C)]
    struct SystemMemory {
        size: u32,
        load: u32,
        total: u64,
        available: u64,
        total_page: u64,
        available_page: u64,
        total_virtual: u64,
        available_virtual: u64,
        extended: u64,
    }
    #[link(name = "kernel32")]
    extern "system" {
        fn CreateToolhelp32Snapshot(flags: u32, pid: u32) -> Handle;
        fn Process32FirstW(snapshot: Handle, entry: *mut Entry) -> i32;
        fn Process32NextW(snapshot: Handle, entry: *mut Entry) -> i32;
        fn OpenProcess(access: u32, inherit: i32, pid: u32) -> Handle;
        fn GetProcessTimes(
            process: Handle,
            created: *mut Time,
            exit: *mut Time,
            kernel: *mut Time,
            user: *mut Time,
        ) -> i32;
        fn K32GetProcessMemoryInfo(process: Handle, counters: *mut Memory, size: u32) -> i32;
        fn GlobalMemoryStatusEx(memory: *mut SystemMemory) -> i32;
        fn CloseHandle(handle: Handle) -> i32;
    }
    struct Owned(Handle);
    impl Drop for Owned {
        fn drop(&mut self) {
            unsafe {
                CloseHandle(self.0);
            }
        }
    }
    unsafe {
        let mut system: SystemMemory = zeroed();
        system.size = size_of::<SystemMemory>() as u32;
        let memory = if GlobalMemoryStatusEx(&mut system) != 0 {
            serde_json::json!({"total_bytes":system.total,"available_bytes":system.available,"load_percent":system.load})
        } else {
            serde_json::Value::Null
        };
        let snapshot = CreateToolhelp32Snapshot(2, 0);
        if snapshot as isize == -1 {
            return (Vec::new(), memory);
        }
        let snapshot = Owned(snapshot);
        let mut entry: Entry = zeroed();
        entry.size = size_of::<Entry>() as u32;
        let mut entries = Vec::new();
        let mut ok = Process32FirstW(snapshot.0, &mut entry);
        while ok != 0 {
            entries.push((entry.pid, entry.parent, entry.threads));
            ok = Process32NextW(snapshot.0, &mut entry);
        }
        let mut owned = std::collections::HashSet::from([std::process::id()]);
        loop {
            let count = owned.len();
            for &(pid, parent, _) in &entries {
                if owned.contains(&parent) && owned.len() < 128 {
                    owned.insert(pid);
                }
            }
            if owned.len() == count {
                break;
            }
        }
        let mut processes = Vec::new();
        for (pid, parent, threads) in entries
            .into_iter()
            .filter(|(pid, _, _)| owned.contains(pid))
        {
            let process = OpenProcess(0x1000 | 0x10, 0, pid);
            if process.is_null() {
                continue;
            }
            let process = Owned(process);
            let (mut created, mut exit, mut kernel, mut user) = (
                Time::default(),
                Time::default(),
                Time::default(),
                Time::default(),
            );
            let mut counters: Memory = zeroed();
            counters.size = size_of::<Memory>() as u32;
            if GetProcessTimes(process.0, &mut created, &mut exit, &mut kernel, &mut user) == 0
                || K32GetProcessMemoryInfo(process.0, &mut counters, size_of::<Memory>() as u32)
                    == 0
            {
                continue;
            }
            processes.push(Process {
                pid,
                parent,
                threads,
                created: created.value(),
                cpu_100ns: kernel.value() + user.value(),
                rss: counters.rss,
                private: counters.private,
            });
        }
        (processes, memory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cpu_units_and_rotation_are_bounded() {
        assert_eq!(cpu_percent(10_000_000, 0, 1.0), 100.0);
        assert_eq!(cpu_percent(0, 10_000_000, 1.0), 0.0);
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("process-test.jsonl");
        for _ in 0..20 {
            append(&path, b"12345\n", 12).unwrap();
        }
        assert!(path.metadata().unwrap().len() <= 12);
        assert!(
            path.with_extension("previous.jsonl")
                .metadata()
                .unwrap()
                .len()
                <= 12
        );
    }
    #[test]
    #[ignore = "writes resource sampling acceptance evidence"]
    fn sampling_acceptance() {
        let directory = PathBuf::from(std::env::var_os("DCL_SAMPLE_EVIDENCE").unwrap());
        let guard = start(&directory).unwrap();
        session("sampling-fixture");
        std::thread::sleep(Duration::from_secs(11));
        drop(guard);
        let path = std::fs::read_dir(directory.join("logs/resources"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let text = std::fs::read_to_string(path).unwrap();
        let rows: Vec<serde_json::Value> = text
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(rows.len() >= 2);
        assert_eq!(rows.last().unwrap()["session"], "sampling-fixture");
        assert!(rows.last().unwrap()["processes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|process| process["cpu_percent"].is_number()));
        println!(
            "rows={} bytes={} sample_ms={}",
            rows.len(),
            text.len(),
            rows.last().unwrap()["sample_ms"]
        );
    }

    #[cfg(windows)]
    #[test]
    fn samples_own_process_without_shells() {
        let (processes, memory) = sample();
        assert!(processes
            .iter()
            .any(|process| process.pid == std::process::id() && process.rss > 0));
        assert!(memory["total_bytes"].as_u64().unwrap() > 0);
    }
}
