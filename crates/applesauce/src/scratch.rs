use applesauce_core::compressor::Kind;
use applesauce_core::{decmpfs, num_blocks};
use std::fs;
use std::io::{self, Cursor, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Arc, Condvar, Mutex};
use tempfile::{NamedTempFile, TempDir, TempPath};

/// Shared by the compressor output buffers and the scratch reservation bound.
pub(crate) const COMPRESSED_BLOCK_CAPACITY: usize = applesauce_core::BLOCK_SIZE + 1024;

#[derive(Debug)]
pub(crate) struct Scratch {
    storage: Storage,
    budget: Arc<Budget>,
}

#[derive(Debug)]
enum Storage {
    Directory {
        directory: TempDir,
        device: u64,
        inode: u64,
    },
    Memory,
}

impl Scratch {
    pub(crate) fn new(directory: &Path, limit: u64) -> io::Result<Self> {
        let budget = Self::budget(limit)?;
        let directory = TempDir::with_prefix_in("applesauce_scratch", directory.canonicalize()?)?;
        let metadata = directory.path().metadata()?;
        Ok(Self {
            storage: Storage::Directory {
                directory,
                device: metadata.dev(),
                inode: metadata.ino(),
            },
            budget,
        })
    }

    pub(crate) fn memory(limit: u64) -> io::Result<Self> {
        Ok(Self {
            storage: Storage::Memory,
            budget: Self::budget(limit)?,
        })
    }

    fn budget(limit: u64) -> io::Result<Arc<Budget>> {
        if limit == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "scratch limit must be greater than zero",
            ));
        }
        Ok(Arc::new(Budget {
            limit,
            used: Mutex::new(Usage::default()),
            available: Condvar::new(),
        }))
    }

    pub(crate) fn is_temp_dir(&self, path: &Path) -> bool {
        match &self.storage {
            Storage::Directory {
                directory,
                device,
                inode,
            } => {
                path.file_name() == directory.path().file_name()
                    && fs::symlink_metadata(path)
                        .is_ok_and(|m| m.dev() == *device && m.ino() == *inode)
            }
            Storage::Memory => false,
        }
    }

    pub(crate) fn reserve(&self, kind: Kind, file_size: u64) -> io::Result<Reservation> {
        self.reserve_bytes(Self::compressed_reservation_size(kind, file_size))
    }

    pub(crate) fn compressed_reservation_size(kind: Kind, file_size: u64) -> u64 {
        let blocks = num_blocks(file_size);
        // Reserve every output block at the compressor's full buffer capacity, plus
        // the format header/trailer and inline xattr. This covers incompressible data
        // and avoids filling the budget with partial files that cannot finish.
        blocks * COMPRESSED_BLOCK_CAPACITY as u64
            + kind.header_size(blocks)
            + decmpfs::ZLIB_TRAILER.len() as u64
            + decmpfs::MAX_XATTR_SIZE as u64
    }

    pub(crate) fn reserve_bytes(&self, bytes: u64) -> io::Result<Reservation> {
        self.budget.reserve(bytes)
    }

    pub(crate) fn reserved_bytes(&self) -> u64 {
        self.budget.used.lock().unwrap().bytes
    }

    pub(crate) fn limit(&self) -> u64 {
        self.budget.limit
    }

    pub(crate) fn stop(&self) {
        self.budget.stop();
    }

    pub(crate) fn is_memory(&self) -> bool {
        matches!(self.storage, Storage::Memory)
    }

    pub(crate) fn payload(&self, limit: u64) -> io::Result<Payload> {
        match &self.storage {
            Storage::Directory { directory, .. } => tempfile::Builder::new()
                .prefix("payload")
                .tempfile_in(directory.path())
                .map(Payload::Disk),
            Storage::Memory => Ok(Payload::Memory(MemoryPayload {
                data: Cursor::new(Vec::new()),
                limit: usize::try_from(limit).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "memory scratch payload is too large",
                    )
                })?,
            })),
        }
    }

    pub(crate) fn reserve_uncompressed(&self, file_size: u64) -> io::Result<Reservation> {
        self.reserve_bytes(file_size)
    }
}

/// Seekable staging storage for resource-fork headers as well as block data.
pub(crate) enum Payload {
    Disk(NamedTempFile),
    Memory(MemoryPayload),
}

impl Payload {
    pub(crate) fn finish(self) -> io::Result<StagedPayload> {
        match self {
            Self::Disk(file) => Ok(StagedPayload::Disk {
                len: file.as_file().metadata()?.len(),
                path: file.into_temp_path(),
            }),
            Self::Memory(payload) => {
                let mut data = payload.data.into_inner();
                data.shrink_to_fit();
                Ok(StagedPayload::Memory(data))
            }
        }
    }
}

impl Write for Payload {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        match self {
            Self::Disk(file) => file.write(data),
            Self::Memory(payload) => payload.write(data),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Disk(file) => file.flush(),
            Self::Memory(payload) => payload.flush(),
        }
    }
}

impl Seek for Payload {
    fn seek(&mut self, position: SeekFrom) -> io::Result<u64> {
        match self {
            Self::Disk(file) => file.seek(position),
            Self::Memory(payload) => payload.data.seek(position),
        }
    }
}

pub(crate) struct MemoryPayload {
    data: Cursor<Vec<u8>>,
    limit: usize,
}

impl Write for MemoryPayload {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let end = self
            .data
            .position()
            .checked_add(bytes.len() as u64)
            .and_then(|end| usize::try_from(end).ok())
            .filter(|&end| end <= self.limit)
            .ok_or_else(|| io::Error::other("memory scratch payload exceeded its reservation"))?;
        let data = self.data.get_mut();
        if end > data.capacity() {
            // Grow geometrically without allowing Vec's spare capacity to exceed
            // the reservation. Allocate lazily and propagate allocation failures.
            let capacity = end.max(data.capacity().saturating_mul(2)).min(self.limit);
            data.try_reserve_exact(capacity - data.len())
                .map_err(io::Error::other)?;
            if data.capacity() > self.limit {
                return Err(io::Error::other(
                    "memory scratch allocation exceeded its reservation",
                ));
            }
        }
        self.data.write(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Disk handles are closed between staging and publication; memory buffers move
/// directly into the publisher queue without copying or spilling to a file.
pub(crate) enum StagedPayload {
    Disk { path: TempPath, len: u64 },
    Memory(Vec<u8>),
}

impl StagedPayload {
    pub(crate) fn len(&self) -> u64 {
        match self {
            Self::Disk { len, .. } => *len,
            Self::Memory(data) => data.len() as u64,
        }
    }

    pub(crate) fn reserved_bytes(&self) -> u64 {
        match self {
            Self::Disk { len, .. } => *len,
            Self::Memory(data) => data.capacity() as u64,
        }
    }

    pub(crate) fn reader(&self) -> io::Result<Box<dyn Read + '_>> {
        match self {
            Self::Disk { path, .. } => Ok(Box::new(fs::File::open(path)?)),
            Self::Memory(data) => Ok(Box::new(Cursor::new(data.as_slice()))),
        }
    }

    pub(crate) fn close(self) -> io::Result<()> {
        match self {
            Self::Disk { path, .. } => path.close(),
            Self::Memory(_) => Ok(()),
        }
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
    fn stop(&self) {
        let mut used = self.used.lock().unwrap();
        used.stopped = true;
        self.available.notify_all();
    }

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
                "scratch staging stopped after an I/O failure",
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

/// Kept until the associated payload has been deleted or freed, including on errors.
pub(crate) struct Reservation {
    budget: Arc<Budget>,
    bytes: u64,
}

impl Reservation {
    pub(crate) fn size(&self) -> u64 {
        self.bytes
    }

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
        self.budget.stop();
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
    fn memory_payload_supports_headers_holes_and_bounded_growth() {
        let scratch = Scratch::memory(32).unwrap();
        assert!(!scratch.is_temp_dir(Path::new(".")));
        let mut reservation = scratch.reserve_bytes(32).unwrap();
        let mut payload = scratch.payload(reservation.size()).unwrap();
        payload.seek(SeekFrom::Start(8)).unwrap();
        payload.write_all(b"payload").unwrap();
        payload.rewind().unwrap();
        payload.write_all(b"head").unwrap();
        payload.seek(SeekFrom::End(0)).unwrap();
        payload.write_all(&[b'x'; 17]).unwrap();
        assert!(payload.write_all(b"!").is_err());
        assert!(payload.seek(SeekFrom::Current(-33)).is_err());
        let payload = payload.finish().unwrap();
        reservation.shrink_to(payload.reserved_bytes());
        assert_eq!(scratch.reserved_bytes(), payload.reserved_bytes());
        let mut bytes = Vec::new();
        payload.reader().unwrap().read_to_end(&mut bytes).unwrap();
        assert_eq!(
            bytes,
            [b"head\0\0\0\0payload".as_slice(), &[b'x'; 17]].concat()
        );
        assert!(matches!(payload, StagedPayload::Memory(_)));
        payload.close().unwrap();
        drop(reservation);
        assert_eq!(scratch.reserved_bytes(), 0);
    }

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
