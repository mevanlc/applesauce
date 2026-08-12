use applesauce_core::{decmpfs, round_to_block_size};
use jwalk::rayon::iter::{ParallelBridge as _, ParallelIterator as _};
use std::ffi::{CStr, CString};
use std::fmt;
use std::fs::Metadata;
use std::io;
use std::os::macos::fs::MetadataExt as _;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;

use crate::progress::{Progress, Task as _};
use crate::volumes::Volumes;
use crate::xattr;
pub use applesauce_core::decmpfs::CompressionType;

pub struct DecmpfsInfo {
    pub compression_type: CompressionType,
    pub attribute_size: u64,
    pub orig_file_size: u64,
}

#[non_exhaustive]
pub struct AfscFileInfo {
    pub is_compressed: bool,
    pub on_disk_size: u64,
    pub stat_size: u64,

    pub xattr_count: u32,
    pub total_xattr_size: u64,

    pub resource_fork_size: Option<u64>,

    pub decmpfs_info: Option<Result<DecmpfsInfo, decmpfs::DecodeError>>,
}

#[non_exhaustive]
pub struct FileInfo {
    pub on_disk_size: u64,
    pub compression_state: FileCompressionState,
}

#[non_exhaustive]
pub enum IncompressibleReason {
    Empty,
    TooLarge(u64),
    IoError(io::Error),
    FsNotSupported,
    HasRequiredXattr,
}

impl fmt::Display for IncompressibleReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IncompressibleReason::Empty => write!(f, "empty file"),
            IncompressibleReason::TooLarge(size) => {
                write!(f, "file too large to compress: {size} bytes")
            }
            IncompressibleReason::IoError(e) => e.fmt(f),
            IncompressibleReason::FsNotSupported => {
                write!(f, "filesystem does not support compression")
            }
            IncompressibleReason::HasRequiredXattr => {
                write!(f, "file has a required xattr for compression already")
            }
        }
    }
}

pub enum FileCompressionState {
    Compressed,
    Compressible,
    Incompressible(IncompressibleReason),
}

impl AfscFileInfo {
    #[must_use]
    pub fn compressed_fraction(&self) -> f64 {
        self.on_disk_size as f64 / self.stat_size as f64
    }
}

#[derive(Debug, Default, Copy, Clone)]
#[non_exhaustive]
pub struct AfscFolderInfo {
    pub num_files: u32,
    pub num_folders: u32,
    pub num_compressed_files: u32,

    pub total_uncompressed_size: u64,
    pub total_compressed_size: u64,
}

impl AfscFolderInfo {
    #[must_use]
    pub fn compressed_fraction(&self) -> f64 {
        self.total_compressed_size as f64 / self.total_uncompressed_size as f64
    }

    #[must_use]
    pub fn compression_savings_fraction(&self) -> f64 {
        1.0 - self.compressed_fraction()
    }
}

pub fn get_recursive(path: &Path) -> io::Result<AfscFolderInfo> {
    get_recursive_with(path, get)
}

pub fn count_recursive_files(path: &Path) -> io::Result<u64> {
    let mut count = 0u64;
    for entry in jwalk::WalkDir::new(path) {
        let entry = entry?;
        #[allow(clippy::filetype_is_file)]
        if entry.file_type().is_file() && entry.metadata()?.nlink() <= 1 {
            count += 1;
        }
    }
    Ok(count)
}

pub fn get_recursive_with_progress<P: Progress + Sync>(
    path: &Path,
    progress: &P,
) -> io::Result<AfscFolderInfo> {
    get_recursive_with(path, |path| get_with_progress(path, progress))
}

fn get_recursive_with<F>(path: &Path, get_file: F) -> io::Result<AfscFolderInfo>
where
    F: Fn(&Path) -> io::Result<AfscFileInfo> + Sync,
{
    jwalk::WalkDir::new(path)
        .into_iter()
        .par_bridge()
        .try_fold(AfscFolderInfo::default, |mut result, entry| {
            let entry = entry?;
            let file_type = entry.file_type();

            #[allow(clippy::filetype_is_file)]
            if file_type.is_file() {
                if entry.metadata()?.nlink() > 1 {
                    return Ok(result);
                }
                let info = get_file(&entry.path())?;
                result.num_files += 1;
                if info.is_compressed {
                    result.num_compressed_files += 1;
                }
                result.total_compressed_size += info.on_disk_size;
                result.total_uncompressed_size += info.stat_size;
            } else if file_type.is_dir() {
                result.num_folders += 1;
            }
            Ok(result)
        })
        .try_reduce(AfscFolderInfo::default, |mut total, partial| {
            total.num_files += partial.num_files;
            total.num_folders += partial.num_folders;
            total.num_compressed_files += partial.num_compressed_files;
            total.total_uncompressed_size += partial.total_uncompressed_size;
            total.total_compressed_size += partial.total_compressed_size;
            Ok(total)
        })
}

pub fn get_with_progress<P: Progress>(path: &Path, progress: &P) -> io::Result<AfscFileInfo> {
    let task = progress.file_task(path, 1);
    let result = get(path);
    task.increment(1);
    result
}

pub fn get_file_info(path: &Path, metadata: &Metadata, volumes: &Volumes) -> FileInfo {
    let compression_info = get_compression_state(path, metadata, volumes);
    let on_disk_size = round_to_block_size(metadata.blocks() * 512, metadata.st_blksize());
    FileInfo {
        on_disk_size,
        compression_state: compression_info,
    }
}

#[tracing::instrument(level = "debug", skip_all)]
pub fn get_compression_state(
    path: &Path,
    metadata: &Metadata,
    volumes: &Volumes,
) -> FileCompressionState {
    if metadata.st_flags() & libc::UF_COMPRESSED != 0 {
        return FileCompressionState::Compressed;
    }

    if metadata.len() == 0 {
        return FileCompressionState::Incompressible(IncompressibleReason::Empty);
    }
    if metadata.len() >= u64::from(u32::MAX) {
        return FileCompressionState::Incompressible(IncompressibleReason::TooLarge(
            metadata.len(),
        ));
    }

    match volumes.supports_compression(path, metadata) {
        Ok(true) => {}
        Ok(false) => {
            return FileCompressionState::Incompressible(IncompressibleReason::FsNotSupported)
        }
        Err(e) => return FileCompressionState::Incompressible(IncompressibleReason::IoError(e)),
    };

    // TODO: Try a local buffer for non-alloc fast path
    let path = match CString::new(path.as_os_str().as_bytes()) {
        Ok(path) => path,
        Err(e) => {
            return FileCompressionState::Incompressible(IncompressibleReason::IoError(e.into()))
        }
    };
    match xattr::is_present(&path, resource_fork::XATTR_NAME) {
        Ok(true) => {
            return FileCompressionState::Incompressible(IncompressibleReason::HasRequiredXattr);
        }
        Ok(false) => {}
        Err(e) => {
            return FileCompressionState::Incompressible(IncompressibleReason::IoError(e));
        }
    };
    match xattr::is_present(&path, decmpfs::XATTR_NAME) {
        Ok(true) => {
            return FileCompressionState::Incompressible(IncompressibleReason::HasRequiredXattr);
        }
        Ok(false) => {}
        Err(e) => {
            return FileCompressionState::Incompressible(IncompressibleReason::IoError(e));
        }
    };

    FileCompressionState::Compressible
}

pub fn get(path: &Path) -> io::Result<AfscFileInfo> {
    let metadata = path.metadata()?;

    let on_disk_size = round_to_block_size(metadata.blocks() * 512, metadata.st_blksize());

    // TODO: Try a local buffer for non-alloc fast path
    let path = CString::new(path.as_os_str().as_bytes())?;

    let mut total_xattr_size = 0;
    let mut xattr_count = 0;
    let mut resource_fork_size = None;
    let mut decmpfs_info = None;
    xattr::with_names(&path, |xattr_name| {
        if xattr_name == decmpfs::XATTR_NAME {
            debug_assert!(decmpfs_info.is_none());
            let info = get_decmpfs_info(&path)?;
            decmpfs_info = Some(info);
        } else {
            let maybe_len = xattr::len(&path, xattr_name)?;
            let len = maybe_len.ok_or_else(|| {
                io::Error::other(format!(
                    "file claimed to have xattr '{}', but it has no len",
                    xattr_name.to_string_lossy()
                ))
            })?;
            let len = u64::try_from(len).unwrap();

            if xattr_name == resource_fork::XATTR_NAME {
                debug_assert!(resource_fork_size.is_none());
                resource_fork_size = Some(len);
            } else {
                xattr_count += 1;
                total_xattr_size += len;
            }
        }

        Ok(())
    })?;

    Ok(AfscFileInfo {
        is_compressed: (metadata.st_flags() & libc::UF_COMPRESSED) == libc::UF_COMPRESSED,
        on_disk_size,
        stat_size: metadata.len(),
        xattr_count,
        total_xattr_size,
        resource_fork_size,
        decmpfs_info,
    })
}

fn get_decmpfs_info(path: &CStr) -> io::Result<Result<DecmpfsInfo, decmpfs::DecodeError>> {
    let maybe_data = xattr::read(path, decmpfs::XATTR_NAME)?;
    let data = maybe_data.ok_or_else(|| io::Error::other("cannot get decmpfs xattr"))?;

    Ok(decmpfs_info_from_bytes(&data))
}

fn decmpfs_info_from_bytes(data: &[u8]) -> Result<DecmpfsInfo, decmpfs::DecodeError> {
    let value = decmpfs::Value::from_data(data)?;
    Ok(DecmpfsInfo {
        compression_type: value.compression_type,
        attribute_size: data.len().try_into().unwrap(),
        orig_file_size: value.uncompressed_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::Task;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Arc;

    #[derive(Default)]
    struct CountingProgress {
        tasks: AtomicU64,
        increments: Arc<AtomicU64>,
    }

    struct CountingTask {
        increments: Arc<AtomicU64>,
    }

    impl Progress for CountingProgress {
        type Task = CountingTask;

        fn error(&self, _path: &Path, _message: &str) {}

        fn file_task(&self, _path: &Path, size: u64) -> Self::Task {
            assert_eq!(size, 1);
            self.tasks.fetch_add(1, Ordering::Relaxed);
            CountingTask {
                increments: Arc::clone(&self.increments),
            }
        }
    }

    impl Task for CountingTask {
        fn increment(&self, amount: u64) {
            self.increments.fetch_add(amount, Ordering::Relaxed);
        }

        fn error(&self, _message: &str) {}
    }

    #[test]
    fn recursive_info_matches_compression_accounting_and_reports_progress() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("one"), b"one").unwrap();
        fs::create_dir(dir.path().join("nested")).unwrap();
        fs::write(dir.path().join("nested/two"), b"two").unwrap();
        fs::write(dir.path().join("hard-link-source"), b"linked").unwrap();
        fs::hard_link(
            dir.path().join("hard-link-source"),
            dir.path().join("hard-link-alias"),
        )
        .unwrap();
        let progress = CountingProgress::default();

        assert_eq!(count_recursive_files(dir.path()).unwrap(), 2);
        let info = get_recursive_with_progress(dir.path(), &progress).unwrap();
        let expected_on_disk_size = get(&dir.path().join("one")).unwrap().on_disk_size
            + get(&dir.path().join("nested/two")).unwrap().on_disk_size;

        assert_eq!(info.num_files, 2);
        assert_eq!(info.total_uncompressed_size, 6);
        assert_eq!(info.total_compressed_size, expected_on_disk_size);
        assert_eq!(progress.tasks.load(Ordering::Relaxed), 2);
        assert_eq!(progress.increments.load(Ordering::Relaxed), 2);
    }
}
