use applesauce_core::compressor::Kind;
use applesauce_core::{decmpfs, num_blocks};
use std::fs;
use std::io;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use tempfile::{NamedTempFile, TempDir};

/// Shared by the compressor output buffers and the scratch reservation bound.
pub(crate) const COMPRESSED_BLOCK_CAPACITY: usize = applesauce_core::BLOCK_SIZE + 1024;

#[derive(Debug)]
pub(crate) struct Scratch {
    directory: TempDir,
    budget: Arc<Budget>,
    device: u64,
    inode: u64,
}

impl Scratch {
    pub(crate) fn new(directory: &Path, limit: u64) -> io::Result<Self> {
        if limit == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "scratch limit must be greater than zero",
            ));
        }
        let directory = TempDir::with_prefix_in("applesauce_scratch", directory.canonicalize()?)?;
        let metadata = directory.path().metadata()?;
        Ok(Self {
            directory,
            budget: Arc::new(Budget {
                limit,
                used: Mutex::new(Usage::default()),
                available: Condvar::new(),
            }),
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }

    pub(crate) fn is_temp_dir(&self, path: &Path) -> bool {
        path.file_name() == self.directory.path().file_name()
            && fs::symlink_metadata(path)
                .is_ok_and(|m| m.dev() == self.device && m.ino() == self.inode)
    }

    pub(crate) fn reserve(&self, kind: Kind, file_size: u64) -> io::Result<Reservation> {
        let blocks = num_blocks(file_size);
        // Reserve every output block at the compressor's full buffer capacity, plus
        // the format header/trailer and inline xattr. This covers incompressible data
        // and avoids filling the budget with partial files that cannot finish.
        let bytes = blocks * COMPRESSED_BLOCK_CAPACITY as u64
            + kind.header_size(blocks)
            + decmpfs::ZLIB_TRAILER.len() as u64
            + decmpfs::MAX_XATTR_SIZE as u64;
        self.budget.reserve(bytes)
    }

    pub(crate) fn tempfile(&self) -> io::Result<NamedTempFile> {
        tempfile::Builder::new()
            .prefix("payload")
            .tempfile_in(self.directory.path())
    }

    pub(crate) fn reserve_uncompressed(&self, file_size: u64) -> io::Result<Reservation> {
        self.budget.reserve(file_size)
    }
}

#[derive(Debug)]
struct Budget {
    limit: u64,
    used: Mutex<Usage>,
    available: Condvar,
}

#[derive(Debug, Default)]
struct Usage {
    bytes: u64,
    stopped: bool,
}

impl Budget {
    fn reserve(self: &Arc<Self>, bytes: u64) -> io::Result<Reservation> {
        if bytes > self.limit {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "file requires a scratch reservation of {bytes} bytes, exceeding the {} byte scratch limit; increase --scratch-limit",
                    self.limit
                ),
            ));
        }
        let mut used = self.used.lock().unwrap();
        while !used.stopped && bytes > self.limit - used.bytes {
            used = self.available.wait(used).unwrap();
        }
        if used.stopped {
            return Err(io::Error::other(
                "scratch staging stopped after a cleanup failure",
            ));
        }
        used.bytes += bytes;
        tracing::debug!(
            reserved = used.bytes,
            limit = self.limit,
            "reserved scratch space"
        );
        Ok(Reservation {
            budget: Arc::clone(self),
            bytes,
        })
    }
}

/// Kept until the associated scratch file has been deleted, including on errors.
pub(crate) struct Reservation {
    budget: Arc<Budget>,
    bytes: u64,
}

impl Reservation {
    pub(crate) fn shrink_to(&mut self, bytes: u64) {
        assert!(
            bytes <= self.bytes,
            "scratch output exceeded its reservation"
        );
        let mut used = self.budget.used.lock().unwrap();
        used.bytes -= self.bytes - bytes;
        self.bytes = bytes;
        self.budget.available.notify_all();
    }

    pub(crate) fn stop(&self) {
        self.budget.used.lock().unwrap().stopped = true;
        self.budget.available.notify_all();
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut used = self.budget.used.lock().unwrap();
        used.bytes -= self.bytes;
        self.budget.available.notify_all();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    #[test]
    fn reservations_wait_for_shrink_and_release() {
        let budget = Arc::new(Budget {
            limit: 100,
            used: Mutex::new(Usage::default()),
            available: Condvar::new(),
        });
        let mut first = budget.reserve(80).unwrap();
        let (tx, rx) = mpsc::channel();
        let other = Arc::clone(&budget);
        let thread = std::thread::spawn(move || {
            let reservation = other.reserve(60).unwrap();
            tx.send(reservation).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        first.shrink_to(40);
        let second = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(budget.used.lock().unwrap().bytes, 100);
        drop(first);
        drop(second);
        thread.join().unwrap();
        assert_eq!(budget.used.lock().unwrap().bytes, 0);
        assert!(budget.reserve(101).is_err());
        assert!(budget.reserve(100).is_ok());
    }

    #[test]
    fn cleanup_failure_wakes_waiters_instead_of_deadlocking() {
        let budget = Arc::new(Budget {
            limit: 100,
            used: Mutex::new(Usage::default()),
            available: Condvar::new(),
        });
        let reservation = budget.reserve(100).unwrap();
        let other = Arc::clone(&budget);
        let (tx, rx) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            tx.send(other.reserve(1).is_err()).unwrap();
        });
        assert!(rx.recv_timeout(Duration::from_millis(50)).is_err());
        reservation.stop();
        assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap());
        thread.join().unwrap();
    }
}
