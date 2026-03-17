//! Simulated in-memory I/O engine for deterministic testing.
//!
//! `SimIo` implements `ringwal::IoEngine` with:
//! - **In-memory filesystem** backed by `HashMap<PathBuf, SimFile>`
//! - **Three-tier write model** per file: `write_buffer` → `kernel_buffer` → durable
//! - **Fault injection** driven by a seeded RNG + `FaultConfig`
//! - **Crash simulation** that discards non-durable data
//!
//! Because DST is single-threaded, interior state uses `RefCell` (not `Mutex`).
//! The `SimIo` handle is `Clone` via `Arc` for compatibility with `IoEngine: Clone`.

use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use rand::rngs::SmallRng;
use rand::{Rng, SeedableRng};

use ringwal::io::{DirEntry, FileHandle, IoEngine, ReadHandle};

use crate::clock::SimClock;
use crate::fault::FaultConfig;

// ── SimFile: three-tier write model ──────────────────────────────────────────

/// In-memory file with three data tiers mirroring real OS behavior.
#[derive(Debug, Clone)]
struct SimFile {
    /// Unflushed application writes (lost on crash AND on flush failure).
    write_buffer: Vec<u8>,
    /// Flushed to "kernel" but not synced (lost on crash, kept on normal operation).
    kernel_buffer: Vec<u8>,
    /// Synced to "disk" — survives crashes.
    durable: Vec<u8>,
    /// Whether the file is in append mode.
    #[allow(dead_code)]
    append_mode: bool,
}

impl SimFile {
    fn new_append() -> Self {
        Self {
            write_buffer: Vec::new(),
            kernel_buffer: Vec::new(),
            durable: Vec::new(),
            append_mode: true,
        }
    }

    fn new_with_data(data: Vec<u8>) -> Self {
        Self {
            write_buffer: Vec::new(),
            kernel_buffer: Vec::new(),
            durable: data,
            append_mode: false,
        }
    }

    /// Total visible size = durable + `kernel_buffer` + `write_buffer`.
    fn visible_len(&self) -> u64 {
        (self.durable.len() + self.kernel_buffer.len() + self.write_buffer.len()) as u64
    }

    /// Durable size only (what survives a crash).
    #[allow(dead_code)]
    fn durable_len(&self) -> u64 {
        self.durable.len() as u64
    }

    /// All readable bytes (durable + `kernel_buffer` + `write_buffer`).
    fn all_bytes(&self) -> Vec<u8> {
        let mut out = self.durable.clone();
        out.extend_from_slice(&self.kernel_buffer);
        out.extend_from_slice(&self.write_buffer);
        out
    }

    /// `flush()`: `write_buffer` → `kernel_buffer`
    fn flush(&mut self) {
        self.kernel_buffer.extend_from_slice(&self.write_buffer);
        self.write_buffer.clear();
    }

    /// `sync_all()` / `sync_data()`: `kernel_buffer` → durable
    fn sync(&mut self) {
        self.flush(); // ensure write_buffer is drained first
        self.durable.extend_from_slice(&self.kernel_buffer);
        self.kernel_buffer.clear();
    }

    /// Crash: discard `write_buffer` + `kernel_buffer`, keep only durable.
    fn crash(&mut self) {
        self.write_buffer.clear();
        self.kernel_buffer.clear();
    }
}

// ── SimFs: the in-memory filesystem ──────────────────────────────────────────

/// Interior state of the simulated filesystem.
#[derive(Debug)]
struct SimFs {
    files: HashMap<PathBuf, SimFile>,
    dirs: HashSet<PathBuf>,
    rng: SmallRng,
    fault_config: FaultConfig,
    clock: SimClock,
    /// Incremented on each crash for diagnostics.
    crash_count: u64,
}

impl SimFs {
    fn new(seed: u64, fault_config: FaultConfig, clock: SimClock) -> Self {
        Self {
            files: HashMap::new(),
            dirs: HashSet::new(),
            rng: SmallRng::seed_from_u64(seed),
            fault_config,
            clock,
            crash_count: 0,
        }
    }

    /// Returns true if the RNG says this fault should fire.
    fn should_fault(&mut self, rate: f64) -> bool {
        if rate <= 0.0 {
            return false;
        }
        if rate >= 1.0 {
            return true;
        }
        self.rng.gen::<f64>() < rate
    }

    /// Simulates a crash: all files lose non-durable data.
    fn crash(&mut self) {
        for file in self.files.values_mut() {
            file.crash();
        }
        self.crash_count += 1;
    }

    fn normalize(path: &Path) -> PathBuf {
        // Simple normalization: strip trailing slashes, canonicalize . and ..
        // For simulation purposes, just use the path as-is but cleaned.
        let s = path.to_string_lossy();
        let s = s.trim_end_matches('/');
        if s.is_empty() {
            PathBuf::from("/")
        } else {
            PathBuf::from(s)
        }
    }
}

// ── SimIo: the IoEngine implementation ───────────────────────────────────────

/// Simulated I/O engine for deterministic testing.
///
/// Wraps `SimFs` in `Arc<RefCell<...>>` so it can be `Clone + Send + Sync`.
///
/// # Safety (Send + Sync)
///
/// `RefCell` is not `Sync`, but DST is strictly single-threaded
/// (`current_thread` tokio runtime). We wrap in a newtype that
/// implements `Send + Sync` via unsafe. This is sound because:
/// 1. All DST tests use `#[tokio::test(flavor = "current_thread")]`
/// 2. `SimIo` is never shared across OS threads
/// 3. The `Arc` is only for `Clone` — there is only one thread.
#[derive(Clone)]
pub struct SimIo {
    inner: Arc<UnsafeRefCell<SimFs>>,
}

/// Newtype wrapper that makes `RefCell<T>` `Send + Sync` for single-threaded DST.
struct UnsafeRefCell<T>(RefCell<T>);

// Safety: DST is single-threaded. See SimIo doc comment.
unsafe impl<T> Send for UnsafeRefCell<T> {}
unsafe impl<T> Sync for UnsafeRefCell<T> {}

impl<T: std::fmt::Debug> std::fmt::Debug for UnsafeRefCell<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.borrow().fmt(f)
    }
}

impl SimIo {
    /// Creates a new simulated I/O engine.
    ///
    /// - `seed`: deterministic RNG seed for fault decisions
    /// - `fault_config`: failure probabilities
    #[must_use] 
    pub fn new(seed: u64, fault_config: FaultConfig) -> Self {
        Self::with_clock(seed, fault_config, SimClock::default())
    }

    /// Creates a `SimIo` with a custom initial clock value.
    pub fn with_clock(seed: u64, fault_config: FaultConfig, clock: SimClock) -> Self {
        Self {
            inner: Arc::new(UnsafeRefCell(RefCell::new(SimFs::new(
                seed,
                fault_config,
                clock,
            )))),
        }
    }

    /// Simulates a crash: all non-durable data is lost.
    ///
    /// After calling this, the filesystem contains only data that was
    /// `sync_all()` / `sync_data()`'d before the crash.
    pub fn crash(&self) {
        self.inner.0.borrow_mut().crash();
    }

    /// Returns the number of crashes that have occurred.
    #[must_use] 
    pub fn crash_count(&self) -> u64 {
        self.inner.0.borrow().crash_count
    }

    /// Advances the simulated clock by `delta` seconds.
    pub fn advance_clock(&self, delta: u64) {
        self.inner.0.borrow().clock.advance(delta);
    }

    /// Returns the current simulated time.
    #[must_use] 
    pub fn now_secs_val(&self) -> u64 {
        self.inner.0.borrow().clock.now_secs()
    }

    /// Returns the durable contents of a file (what survives a crash).
    /// Useful for test assertions.
    #[must_use] 
    pub fn durable_bytes(&self, path: &Path) -> Option<Vec<u8>> {
        let fs = self.inner.0.borrow();
        let norm = SimFs::normalize(path);
        fs.files.get(&norm).map(|f| f.durable.clone())
    }

    /// Returns the fault config (for diagnostic printing on failure).
    #[must_use] 
    pub fn fault_config(&self) -> FaultConfig {
        self.inner.0.borrow().fault_config.clone()
    }

    fn borrow(&self) -> std::cell::Ref<'_, SimFs> {
        self.inner.0.borrow()
    }

    fn borrow_mut(&self) -> std::cell::RefMut<'_, SimFs> {
        self.inner.0.borrow_mut()
    }
}

impl IoEngine for SimIo {
    type FileHandle = SimFileHandle;
    type ReadHandle = SimReadHandle;

    fn open_append(&self, path: &Path, _direct_io: bool) -> io::Result<SimFileHandle> {
        let mut fs = self.borrow_mut();
        let norm = SimFs::normalize(path);

        // Ensure parent directory exists
        if let Some(parent) = norm.parent() {
            if !parent.as_os_str().is_empty() && !fs.dirs.contains(parent) {
                return Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("parent directory does not exist: {}", parent.display()),
                ));
            }
        }

        // Create or reopen the file
        let file = fs.files.entry(norm.clone()).or_insert_with(SimFile::new_append);
        let initial_size = file.visible_len();
        // For an existing file being re-opened in append mode, we need to
        // make sure previously durable+kernel data is the baseline.
        // The file keeps its state across opens.

        Ok(SimFileHandle {
            io: self.clone(),
            path: norm,
            // Track size for metadata_len()
            size: initial_size,
        })
    }

    fn open_read(&self, path: &Path) -> io::Result<SimReadHandle> {
        let fs = self.borrow();
        let norm = SimFs::normalize(path);

        let read_fail_rate = fs.fault_config.read_fail_rate;
        if read_fail_rate > 0.0 {
            drop(fs);
            let mut fs = self.borrow_mut();
            if fs.should_fault(read_fail_rate) {
                return Err(io::Error::other(
                    "simulated read open failure",
                ));
            }
            match fs.files.get(&norm) {
                Some(file) => {
                    let data = file.all_bytes();
                    let len = data.len() as u64;
                    Ok(SimReadHandle {
                        data,
                        pos: 0,
                        len,
                    })
                }
                None => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("file not found: {}", norm.display()),
                )),
            }
        } else {
            match fs.files.get(&norm) {
                Some(file) => {
                    let data = file.all_bytes();
                    let len = data.len() as u64;
                    Ok(SimReadHandle {
                        data,
                        pos: 0,
                        len,
                    })
                }
                None => Err(io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("file not found: {}", norm.display()),
                )),
            }
        }
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        let mut fs = self.borrow_mut();
        let norm = SimFs::normalize(path);
        // Add the dir and all ancestors
        let mut current = norm.as_path();
        loop {
            fs.dirs.insert(current.to_path_buf());
            match current.parent() {
                Some(p) if !p.as_os_str().is_empty() => current = p,
                _ => break,
            }
        }
        Ok(())
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        let fs = self.borrow();
        let norm = SimFs::normalize(path);

        if !fs.dirs.contains(&norm) {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("directory not found: {}", norm.display()),
            ));
        }

        let mut entries = Vec::new();
        for file_path in fs.files.keys() {
            if let Some(parent) = file_path.parent() {
                if SimFs::normalize(parent) == norm {
                    if let Some(name) = file_path.file_name() {
                        entries.push(DirEntry {
                            file_name: name.to_string_lossy().into_owned(),
                            path: file_path.clone(),
                        });
                    }
                }
            }
        }
        Ok(entries)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        let mut fs = self.borrow_mut();
        let norm = SimFs::normalize(path);
        fs.files.remove(&norm);
        Ok(()) // always Ok per IoEngine contract
    }

    fn write_file_bytes(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        let mut fs = self.borrow_mut();
        let norm = SimFs::normalize(path);

        // Atomic write: directly to durable (mimics fs::write + fsync)
        let file = SimFile::new_with_data(data.to_vec());
        fs.files.insert(norm, file);
        Ok(())
    }

    fn read_file_bytes(&self, path: &Path) -> io::Result<Vec<u8>> {
        let fs = self.borrow();
        let norm = SimFs::normalize(path);
        match fs.files.get(&norm) {
            Some(file) => Ok(file.all_bytes()),
            None => Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("file not found: {}", norm.display()),
            )),
        }
    }

    fn exists(&self, path: &Path) -> bool {
        let fs = self.borrow();
        let norm = SimFs::normalize(path);
        fs.files.contains_key(&norm) || fs.dirs.contains(&norm)
    }

    fn now_secs(&self) -> u64 {
        self.borrow().clock.now_secs()
    }
}

// ── SimFileHandle ────────────────────────────────────────────────────────────

/// Writable file handle for the simulated filesystem.
///
/// Each handle holds a reference to the `SimIo` and the path. Writes go
/// through the `SimIo` to update the `SimFile`'s `write_buffer`.
pub struct SimFileHandle {
    io: SimIo,
    path: PathBuf,
    size: u64,
}

impl Write for SimFileHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut fs = self.io.borrow_mut();

        // Extract rates to avoid borrow conflicts with should_fault(&mut self)
        let write_fail_rate = fs.fault_config.write_fail_rate;
        let partial_write_rate = fs.fault_config.partial_write_rate;
        let crash_probability = fs.fault_config.crash_probability;

        // Check for write failure
        if fs.should_fault(write_fail_rate) {
            return Err(io::Error::other(
                "simulated write failure",
            ));
        }

        // Check for partial write
        let write_len = if fs.should_fault(partial_write_rate) {
            if buf.is_empty() {
                0
            } else {
                // Write between 1 and buf.len()-1 bytes (at least 1 to make progress)
                let max = buf.len().max(2) - 1;
                fs.rng.gen_range(1..=max).min(buf.len())
            }
        } else {
            buf.len()
        };

        let file = fs
            .files
            .get_mut(&self.path)
            .expect("SimFileHandle: file disappeared");
        file.write_buffer.extend_from_slice(&buf[..write_len]);
        self.size += write_len as u64;

        // Check for post-op crash
        if fs.should_fault(crash_probability) {
            fs.crash();
        }

        Ok(write_len)
    }

    fn flush(&mut self) -> io::Result<()> {
        let mut fs = self.io.borrow_mut();
        let file = fs
            .files
            .get_mut(&self.path)
            .expect("SimFileHandle: file disappeared");
        file.flush();
        Ok(())
    }
}

impl FileHandle for SimFileHandle {
    fn sync_all(&mut self) -> io::Result<()> {
        let mut fs = self.io.borrow_mut();

        let fsync_fail_rate = fs.fault_config.fsync_fail_rate;
        let crash_probability = fs.fault_config.crash_probability;

        // Check for fsync failure
        if fs.should_fault(fsync_fail_rate) {
            return Err(io::Error::other(
                "simulated fsync failure",
            ));
        }

        let file = fs
            .files
            .get_mut(&self.path)
            .expect("SimFileHandle: file disappeared");
        file.sync();

        // Check for post-sync crash
        if fs.should_fault(crash_probability) {
            fs.crash();
        }

        Ok(())
    }

    fn sync_data(&mut self) -> io::Result<()> {
        // In simulation, sync_data behaves the same as sync_all
        self.sync_all()
    }

    fn try_clone_file(&self) -> io::Result<std::fs::File> {
        // SimIo doesn't support real fd cloning — pipelined modes are
        // out of scope for Phase 3. Return an error that prevents
        // accidentally using pipelined modes in DST.
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "SimIo does not support try_clone_file (pipelined modes not supported in DST)",
        ))
    }

    fn metadata_len(&self) -> io::Result<u64> {
        let fs = self.io.borrow();
        match fs.files.get(&self.path) {
            Some(file) => Ok(file.visible_len()),
            None => Ok(0),
        }
    }
}

// ── SimReadHandle ────────────────────────────────────────────────────────────

/// Readable file handle backed by an in-memory snapshot.
///
/// Takes a snapshot of the file's data at open time so reads are
/// consistent even if the file is modified concurrently (which
/// shouldn't happen in single-threaded DST, but is safe regardless).
#[derive(Debug)]
pub struct SimReadHandle {
    data: Vec<u8>,
    pos: usize,
    len: u64,
}

impl Read for SimReadHandle {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let remaining = &self.data[self.pos..];
        let n = remaining.len().min(buf.len());
        buf[..n].copy_from_slice(&remaining[..n]);
        self.pos += n;
        Ok(n)
    }
}

impl ReadHandle for SimReadHandle {
    fn metadata_len(&self) -> io::Result<u64> {
        Ok(self.len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_read_roundtrip() {
        let sim = SimIo::new(42, FaultConfig::none());
        sim.create_dir_all(Path::new("/wal")).unwrap();

        let path = Path::new("/wal/test.log");
        {
            let mut fh = sim.open_append(path, false).unwrap();
            fh.write_all(b"hello ").unwrap();
            fh.write_all(b"world").unwrap();
            fh.sync_all().unwrap();
        }

        let mut rh = sim.open_read(path).unwrap();
        let mut buf = Vec::new();
        rh.read_to_end(&mut buf).unwrap();
        assert_eq!(buf, b"hello world");
    }

    #[test]
    fn crash_discards_unsynced() {
        let sim = SimIo::new(42, FaultConfig::none());
        sim.create_dir_all(Path::new("/wal")).unwrap();

        let path = Path::new("/wal/test.log");
        {
            let mut fh = sim.open_append(path, false).unwrap();
            // Write and sync some data
            fh.write_all(b"durable").unwrap();
            fh.sync_all().unwrap();
            // Write more but only flush (not sync)
            fh.write_all(b"-kernel").unwrap();
            fh.flush().unwrap();
            // Write even more (not even flushed)
            fh.write_all(b"-buffer").unwrap();
        }

        // Before crash: all data visible
        let data = sim.read_file_bytes(path).unwrap();
        assert_eq!(data, b"durable-kernel-buffer");

        // Crash!
        sim.crash();

        // After crash: only durable data survives
        let data = sim.read_file_bytes(path).unwrap();
        assert_eq!(data, b"durable");
    }

    #[test]
    fn crash_preserves_durable_only() {
        let sim = SimIo::new(42, FaultConfig::none());
        sim.create_dir_all(Path::new("/wal")).unwrap();

        let path = Path::new("/wal/seg.log");
        {
            let mut fh = sim.open_append(path, false).unwrap();
            fh.write_all(b"AAA").unwrap();
            fh.sync_all().unwrap(); // durable: "AAA"
            fh.write_all(b"BBB").unwrap();
            fh.flush().unwrap(); // kernel: "BBB"
            fh.write_all(b"CCC").unwrap(); // write_buffer: "CCC"
        }

        assert_eq!(sim.durable_bytes(path), Some(b"AAA".to_vec()));

        sim.crash();
        let data = sim.read_file_bytes(path).unwrap();
        assert_eq!(data, b"AAA");
    }

    #[test]
    fn dir_operations() {
        let sim = SimIo::new(42, FaultConfig::none());

        assert!(!sim.exists(Path::new("/wal")));
        sim.create_dir_all(Path::new("/wal/subdir")).unwrap();
        assert!(sim.exists(Path::new("/wal")));
        assert!(sim.exists(Path::new("/wal/subdir")));

        // Create files in /wal
        sim.write_file_bytes(Path::new("/wal/a.log"), b"a").unwrap();
        sim.write_file_bytes(Path::new("/wal/b.log"), b"b").unwrap();

        let entries = sim.read_dir(Path::new("/wal")).unwrap();
        let mut names: Vec<_> = entries.iter().map(|e| e.file_name.clone()).collect();
        names.sort();
        assert_eq!(names, vec!["a.log", "b.log"]);
    }

    #[test]
    fn remove_file_idempotent() {
        let sim = SimIo::new(42, FaultConfig::none());
        sim.create_dir_all(Path::new("/wal")).unwrap();
        sim.write_file_bytes(Path::new("/wal/x"), b"data").unwrap();
        assert!(sim.exists(Path::new("/wal/x")));
        sim.remove_file(Path::new("/wal/x")).unwrap();
        assert!(!sim.exists(Path::new("/wal/x")));
        // Second remove is OK
        sim.remove_file(Path::new("/wal/x")).unwrap();
    }

    #[test]
    fn write_file_bytes_is_durable() {
        let sim = SimIo::new(42, FaultConfig::none());
        sim.create_dir_all(Path::new("/wal")).unwrap();

        sim.write_file_bytes(Path::new("/wal/ckpt"), b"checkpoint").unwrap();
        sim.crash();
        let data = sim.read_file_bytes(Path::new("/wal/ckpt")).unwrap();
        assert_eq!(data, b"checkpoint");
    }

    #[test]
    fn clock_integration() {
        let sim = SimIo::with_clock(42, FaultConfig::none(), SimClock::new(1000));
        assert_eq!(sim.now_secs_val(), 1000);
        sim.advance_clock(60);
        assert_eq!(sim.now_secs_val(), 1060);

        // IoEngine::now_secs() returns the same value
        assert_eq!(IoEngine::now_secs(&sim), 1060);
    }

    #[test]
    fn open_read_not_found() {
        let sim = SimIo::new(42, FaultConfig::none());
        let err = sim.open_read(Path::new("/nope")).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn append_reopen() {
        let sim = SimIo::new(42, FaultConfig::none());
        sim.create_dir_all(Path::new("/wal")).unwrap();

        let path = Path::new("/wal/seg.log");
        {
            let mut fh = sim.open_append(path, false).unwrap();
            fh.write_all(b"first").unwrap();
            fh.sync_all().unwrap();
        }
        // Reopen and append more
        {
            let mut fh = sim.open_append(path, false).unwrap();
            fh.write_all(b"-second").unwrap();
            fh.sync_all().unwrap();
        }

        let data = sim.read_file_bytes(path).unwrap();
        assert_eq!(data, b"first-second");
    }

    #[test]
    fn metadata_len_tracks_writes() {
        let sim = SimIo::new(42, FaultConfig::none());
        sim.create_dir_all(Path::new("/wal")).unwrap();

        let path = Path::new("/wal/seg.log");
        let mut fh = sim.open_append(path, false).unwrap();
        assert_eq!(fh.metadata_len().unwrap(), 0);

        fh.write_all(b"12345").unwrap();
        assert_eq!(fh.metadata_len().unwrap(), 5);

        fh.write_all(b"67890").unwrap();
        assert_eq!(fh.metadata_len().unwrap(), 10);
    }

    #[test]
    fn fault_write_failure() {
        // 100% write failure rate
        let fc = FaultConfig::builder().write_fail_rate(1.0).build();
        let sim = SimIo::new(42, fc);
        sim.create_dir_all(Path::new("/wal")).unwrap();

        let path = Path::new("/wal/test.log");
        let mut fh = sim.open_append(path, false).unwrap();
        let err = fh.write(b"data").unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);
    }

    #[test]
    fn fault_fsync_failure() {
        // 100% fsync failure rate
        let fc = FaultConfig::builder().fsync_fail_rate(1.0).build();
        let sim = SimIo::new(42, fc);
        sim.create_dir_all(Path::new("/wal")).unwrap();

        let path = Path::new("/wal/test.log");
        let mut fh = sim.open_append(path, false).unwrap();
        fh.write_all(b"data").unwrap();
        let err = fh.sync_all().unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Other);

        // Data should still be in write_buffer/kernel_buffer but NOT durable
        assert_eq!(sim.durable_bytes(Path::new("/wal/test.log")), Some(vec![]));
    }
}
