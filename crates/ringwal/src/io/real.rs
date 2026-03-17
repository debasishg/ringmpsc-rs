//! Production I/O backend wrapping `std::fs`.
//!
//! All operations map 1:1 to standard library calls with zero overhead.

use std::io::{self, BufWriter, Read, Write};
use std::path::Path;

use super::{DirEntry, FileHandle, IoEngine, ReadHandle};

// ── Direct I/O helpers (platform-conditional) ────────────────────────────────

#[cfg(target_os = "macos")]
fn set_direct_io(file: &std::fs::File) -> io::Result<()> {
    use std::os::unix::io::AsRawFd;
    // Safety: fd is a valid open file descriptor; F_NOCACHE is a safe flag
    // that only affects caching behaviour, not file integrity.
    let ret = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1) };
    if ret == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn set_direct_io(_file: &std::fs::File) -> io::Result<()> {
    // O_DIRECT requires block-aligned writes — not yet implemented.
    // See docs/DIRECT_IO.md for the follow-up plan.
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn set_direct_io(_file: &std::fs::File) -> io::Result<()> {
    Ok(())
}

// ── RealIo ───────────────────────────────────────────────────────────────────

/// Production I/O engine: delegates to `std::fs` and `std::time`.
#[derive(Debug, Clone, Copy)]
pub struct RealIo;

impl IoEngine for RealIo {
    type FileHandle = RealFileHandle;
    type ReadHandle = RealReadHandle;

    fn open_append(&self, path: &Path, direct_io: bool) -> io::Result<RealFileHandle> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let size = file.metadata()?.len();
        if direct_io {
            set_direct_io(&file)?;
        }
        Ok(RealFileHandle {
            writer: BufWriter::new(file),
            size,
        })
    }

    fn open_read(&self, path: &Path) -> io::Result<RealReadHandle> {
        let file = std::fs::File::open(path)?;
        let len = file.metadata()?.len();
        Ok(RealReadHandle { file, len })
    }

    fn create_dir_all(&self, path: &Path) -> io::Result<()> {
        std::fs::create_dir_all(path)
    }

    fn read_dir(&self, path: &Path) -> io::Result<Vec<DirEntry>> {
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let file_name = entry.file_name().to_string_lossy().into_owned();
            let path = entry.path();
            entries.push(DirEntry { file_name, path });
        }
        Ok(entries)
    }

    fn remove_file(&self, path: &Path) -> io::Result<()> {
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(e),
        }
    }

    fn write_file_bytes(&self, path: &Path, data: &[u8]) -> io::Result<()> {
        std::fs::write(path, data)?;
        // Fsync the written file for durability
        let f = std::fs::File::open(path)?;
        f.sync_all()?;
        Ok(())
    }

    fn read_file_bytes(&self, path: &Path) -> io::Result<Vec<u8>> {
        std::fs::read(path)
    }

    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn now_secs(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
    }
}

// ── RealFileHandle ───────────────────────────────────────────────────────────

/// Writable file handle backed by `BufWriter<File>`.
pub struct RealFileHandle {
    writer: BufWriter<std::fs::File>,
    size: u64,
}

impl Write for RealFileHandle {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.writer.write(buf)?;
        self.size += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

impl FileHandle for RealFileHandle {
    fn sync_all(&mut self) -> io::Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_all()
    }

    fn sync_data(&mut self) -> io::Result<()> {
        self.writer.flush()?;
        self.writer.get_ref().sync_data()
    }

    fn try_clone_file(&self) -> io::Result<std::fs::File> {
        self.writer.get_ref().try_clone()
    }

    fn metadata_len(&self) -> io::Result<u64> {
        Ok(self.size)
    }
}

// ── RealReadHandle ───────────────────────────────────────────────────────────

/// Readable file handle backed by `std::fs::File`.
pub struct RealReadHandle {
    file: std::fs::File,
    len: u64,
}

impl Read for RealReadHandle {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }
}

impl ReadHandle for RealReadHandle {
    fn metadata_len(&self) -> io::Result<u64> {
        Ok(self.len)
    }
}
