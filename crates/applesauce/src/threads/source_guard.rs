use crate::progress::SkipReason;
use crate::threads::Context;
use std::collections::HashSet;
use std::fs::Metadata;
use std::io;
use std::mem::{size_of, MaybeUninit};
use std::os::macos::fs::MetadataExt as _;
use std::os::unix::fs::MetadataExt as _;
use std::sync::Mutex;
use std::time::{Duration, Instant};

// These two system flags are defined by the macOS SDK but not by libc.
const SF_NOUNLINK: u32 = 0x0010_0000;
const SF_RESTRICTED: u32 = 0x0008_0000;
pub(super) const PROTECTED_FLAGS: u32 = libc::UF_APPEND
    | libc::UF_IMMUTABLE
    | libc::SF_APPEND
    | libc::SF_IMMUTABLE
    | SF_NOUNLINK
    | SF_RESTRICTED;

pub(super) fn is_protected(metadata: &Metadata) -> bool {
    metadata.st_flags() & PROTECTED_FLAGS != 0
}

pub(super) fn same_source(current: &Metadata, original: &Metadata) -> bool {
    current.is_file()
        && current.dev() == original.dev()
        && current.ino() == original.ino()
        && current.len() == original.len()
        && current.mtime() == original.mtime()
        && current.mtime_nsec() == original.mtime_nsec()
        && current.ctime() == original.ctime()
        && current.ctime_nsec() == original.ctime_nsec()
        && current.nlink() == original.nlink()
        && current.st_flags() == original.st_flags()
}

fn current_source(context: &Context) -> io::Result<Option<SkipReason>> {
    let current = context.path.symlink_metadata()?;
    if is_protected(&current) {
        return Ok(Some(SkipReason::ProtectedFlags));
    }
    if !same_source(&current, &context.orig_metadata) {
        return Err(io::Error::other("source file changed after staging"));
    }
    Ok(None)
}

/// Recheck the source after staging and shortly before replacing its path.
/// The handle snapshot reduces risk but cannot exclude a writer that opens later.
pub(super) fn before_publish(context: &Context) -> io::Result<Option<SkipReason>> {
    if let Some(reason) = current_source(context)? {
        return Ok(Some(reason));
    }
    match context
        .operation
        .open_writers
        .has_writer(&context.orig_metadata)
    {
        Ok(true) => return Ok(Some(SkipReason::OpenWriter)),
        Ok(false) => {}
        Err(error) => return Ok(Some(SkipReason::OpenWriterCheckFailed(error))),
    }
    current_source(context)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct FileId {
    device: u32,
    inode: u64,
}

impl FileId {
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            // macOS dev_t is 32-bit; MetadataExt widens it to u64.
            device: metadata.dev() as u32,
            inode: metadata.ino(),
        }
    }
}

const REFRESH_INTERVAL: Duration = Duration::from_secs(5);

#[derive(Debug, Default)]
pub(super) struct OpenWriters {
    snapshot: Mutex<Option<Snapshot>>,
}

#[derive(Debug)]
struct Snapshot {
    collected_at: Instant,
    result: Result<HashSet<FileId>, String>,
}

impl OpenWriters {
    pub(super) fn new() -> Self {
        Self::default()
    }

    fn has_writer(&self, metadata: &Metadata) -> io::Result<bool> {
        let mut snapshot = self.snapshot.lock().unwrap();
        if snapshot
            .as_ref()
            .is_none_or(|snapshot| snapshot.collected_at.elapsed() >= REFRESH_INTERVAL)
        {
            let result = collect_open_writers().map_err(|error| error.to_string());
            *snapshot = Some(Snapshot {
                collected_at: Instant::now(),
                result,
            });
        }
        match &snapshot.as_ref().unwrap().result {
            Ok(writers) => Ok(writers.contains(&FileId::from_metadata(metadata))),
            Err(message) => Err(io::Error::other(message.clone())),
        }
    }
}

// Values and layouts from macOS libproc.h, sys/proc_info.h, and sys/fcntl.h.
// libproc marks its process-inspection interfaces private and subject to change.
const PROC_PIDFDVNODEINFO: libc::c_int = 1;
const FWRITE: u32 = 0x2; // fi_openflags uses kernel FREAD/FWRITE, not O_ACCMODE.

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct ProcFdInfo {
    fd: libc::c_int,
    kind: u32,
}

#[repr(C)]
struct ProcFileInfo {
    open_flags: u32,
    status: u32,
    offset: libc::off_t,
    kind: i32,
    guard_flags: u32,
}

#[repr(C)]
struct VnodeFdInfo {
    file: ProcFileInfo,
    vnode: libc::vnode_info,
}

fn byte_size<T>(items: usize) -> io::Result<libc::c_int> {
    items
        .checked_mul(size_of::<T>())
        .and_then(|bytes| libc::c_int::try_from(bytes).ok())
        .ok_or_else(|| io::Error::other("libproc buffer is too large"))
}

fn list_pids() -> io::Result<Vec<libc::pid_t>> {
    // SAFETY: null and zero request the number of live processes.
    let count = unsafe { libc::proc_listallpids(std::ptr::null_mut(), 0) };
    if count <= 0 {
        return Err(io::Error::last_os_error());
    }
    let mut capacity = count as usize + 32;
    loop {
        let mut pids = vec![0; capacity];
        let buffer_size = byte_size::<libc::pid_t>(capacity)?;
        // SAFETY: pids has buffer_size bytes and the API writes pid_t entries.
        let count = unsafe { libc::proc_listallpids(pids.as_mut_ptr().cast(), buffer_size) };
        if count <= 0 {
            return Err(io::Error::last_os_error());
        }
        if count as usize == capacity {
            capacity *= 2;
            continue;
        }
        pids.truncate(count as usize);
        return Ok(pids);
    }
}

fn list_fds(pid: libc::pid_t) -> io::Result<Vec<ProcFdInfo>> {
    // SAFETY: null and zero request the number of bytes needed for the FD list.
    let needed =
        unsafe { libc::proc_pidinfo(pid, libc::PROC_PIDLISTFDS, 0, std::ptr::null_mut(), 0) };
    if needed <= 0 {
        return Err(io::Error::last_os_error());
    }
    let mut capacity = needed as usize / size_of::<ProcFdInfo>() + 8;
    loop {
        let mut fds = vec![ProcFdInfo::default(); capacity];
        let buffer_size = byte_size::<ProcFdInfo>(capacity)?;
        // SAFETY: fds has buffer_size bytes and the flavor writes proc_fdinfo entries.
        let count = unsafe {
            libc::proc_pidinfo(
                pid,
                libc::PROC_PIDLISTFDS,
                0,
                fds.as_mut_ptr().cast(),
                buffer_size,
            )
        };
        if count <= 0 {
            return Err(io::Error::last_os_error());
        }
        if count % size_of::<ProcFdInfo>() as i32 != 0 {
            return Err(io::Error::other("libproc returned partial FD data"));
        }
        if count == buffer_size {
            capacity *= 2;
            continue;
        }
        fds.truncate(count as usize / size_of::<ProcFdInfo>());
        return Ok(fds);
    }
}

fn inspect_fd(pid: libc::pid_t, fd: libc::c_int) -> io::Result<VnodeFdInfo> {
    let mut info = MaybeUninit::<VnodeFdInfo>::uninit();
    let size = byte_size::<VnodeFdInfo>(1)?;
    // SAFETY: info has size bytes; a full return initializes the whole structure.
    let count = unsafe {
        libc::proc_pidfdinfo(pid, fd, PROC_PIDFDVNODEINFO, info.as_mut_ptr().cast(), size)
    };
    if count != size {
        return Err(if count < 0 {
            io::Error::last_os_error()
        } else {
            io::Error::other("libproc returned incomplete vnode FD data")
        });
    }
    // SAFETY: proc_pidfdinfo filled the whole structure above.
    Ok(unsafe { info.assume_init() })
}

fn collect_open_writers() -> io::Result<HashSet<FileId>> {
    let mut writers = HashSet::new();
    let mut inspected_vnodes = 0;
    for pid in list_pids()? {
        let fds = match list_fds(pid) {
            Ok(fds) => fds,
            // Some processes deny inspection or exit during the scan. The
            // result is best effort; failing the entire run would skip all files.
            Err(_) => continue,
        };
        for fd in fds {
            if fd.kind != libc::PROX_FDTYPE_VNODE as u32 {
                continue;
            }
            let Ok(info) = inspect_fd(pid, fd.fd) else {
                continue;
            };
            inspected_vnodes += 1;
            if info.file.open_flags & FWRITE != 0 {
                writers.insert(FileId {
                    device: info.vnode.vi_stat.vst_dev,
                    inode: info.vnode.vi_stat.vst_ino,
                });
            }
        }
    }
    if inspected_vnodes == 0 {
        return Err(io::Error::other("libproc could not inspect any open files"));
    }
    Ok(writers)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::{File, OpenOptions};

    #[test]
    fn snapshot_distinguishes_readers_from_writers() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("source");
        std::fs::write(&path, b"source").unwrap();
        let metadata = path.metadata().unwrap();
        let reader = File::open(&path).unwrap();
        assert!(!OpenWriters::new().has_writer(&metadata).unwrap());
        let writer = OpenOptions::new().append(true).open(&path).unwrap();
        assert!(OpenWriters::new().has_writer(&metadata).unwrap());
        drop(writer);
        drop(reader);
        assert!(!OpenWriters::new().has_writer(&metadata).unwrap());
    }
}
