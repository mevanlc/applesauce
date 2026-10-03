#![warn(unsafe_op_in_unsafe_fn)]
#![warn(clippy::undocumented_unsafe_blocks)]
#![warn(clippy::cast_lossless)]
#![warn(clippy::cast_ptr_alignment)]
#![warn(clippy::clone_on_ref_ptr)]
#![warn(clippy::cloned_instead_of_copied)]
#![warn(clippy::debug_assert_with_mut_call)]
#![warn(clippy::filetype_is_file)]
#![warn(clippy::match_same_arms)]
extern crate core;

#[cfg(not(any(target_os = "macos", target_os = "ios")))]
compile_error!("applesauce only works on macos/ios");

pub mod info;
pub mod progress;
pub use applesauce_core::compressor;

mod rfork_storage;
mod scan;
mod scratch;
mod seq_queue;
mod threads;
mod times;
mod volumes;
mod xattr;

use libc::c_char;
use std::ffi::CStr;
use std::fs::{File, Metadata};
use std::io::prelude::*;
use std::mem::MaybeUninit;
use std::num::NonZeroUsize;
use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::sync::atomic::AtomicU64;
use std::{io, mem, ptr};
use tracing::warn;

use crate::info::{FileCompressionState, FileInfo};
use crate::progress::Progress;
use crate::threads::{BackgroundThreads, Mode};
#[cfg(test)]
use applesauce_core::compressor::Kind;

const fn c_char_bytes(chars: &[c_char]) -> &[u8] {
    assert!(size_of::<c_char>() == size_of::<u8>());
    assert!(align_of::<c_char>() == align_of::<u8>());
    // SAFETY: c_char is the same layout as u8
    unsafe { mem::transmute(chars) }
}

fn cstr_from_bytes_until_null(bytes: &[c_char]) -> Option<&CStr> {
    let bytes = c_char_bytes(bytes);
    let pos = memchr::memchr(0, bytes)?;
    CStr::from_bytes_with_nul(&bytes[..=pos]).ok()
}

fn vol_supports_compression_cap(mnt_root: &CStr) -> io::Result<bool> {
    #[repr(C)]
    struct VolAttrs {
        length: u32,
        vol_attrs: libc::vol_capabilities_attr_t,
    }
    const IDX: usize = libc::VOL_CAPABILITIES_FORMAT;
    const MASK: libc::attrgroup_t = libc::VOL_CAP_FMT_DECMPFS_COMPRESSION;

    // SAFETY: All fields are simple integers which can be zero-initialized
    let mut attrs = unsafe { MaybeUninit::<libc::attrlist>::zeroed().assume_init() };
    attrs.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
    attrs.volattr = libc::ATTR_VOL_CAPABILITIES;

    let mut vol_attrs = MaybeUninit::<VolAttrs>::uninit();
    // SAFETY:
    // `mnt_root` is a valid pointer, and is null terminated
    // attrs is a valid pointer to initialized memory of the correct type
    // vol_attrs is a valid pointer, and its size is passed as the size of the buffer
    let rc = unsafe {
        libc::getattrlist(
            mnt_root.as_ptr(),
            ptr::addr_of_mut!(attrs).cast(),
            vol_attrs.as_mut_ptr().cast(),
            size_of_val(&vol_attrs),
            0,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: getattrlist returned success
    let vol_attrs = unsafe { vol_attrs.assume_init_ref() };
    if vol_attrs.length != u32::try_from(size_of::<VolAttrs>()).unwrap() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "getattrlist returned bad size",
        ));
    }

    Ok(vol_attrs.vol_attrs.valid[IDX] & vol_attrs.vol_attrs.capabilities[IDX] & MASK != 0)
}

#[tracing::instrument(level = "trace", skip_all, fields(flags), err)]
fn set_flags(file: &File, flags: libc::c_uint) -> io::Result<()> {
    let rc =
        // SAFETY: fd is valid
        unsafe { libc::fchflags(file.as_raw_fd(), flags) };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[derive(Debug, Default)]
pub struct Stats {
    /// Total number of files scanned
    pub files: AtomicU64,
    /// Total of all file sizes (uncompressed)
    pub total_file_sizes: AtomicU64,

    pub compressed_size_start: AtomicU64,
    /// Total of all file sizes (after compression) after performing this operation
    pub compressed_size_final: AtomicU64,
    /// Number of files that were compressed before performing this operation
    pub compressed_file_count_start: AtomicU64,
    /// Number of files that were compressed after performing this operation
    pub compressed_file_count_final: AtomicU64,

    /// Number of files that were incompressible (only present when compressing)
    pub incompressible_file_count: AtomicU64,
}

impl Stats {
    fn add_start_file(&self, metadata: &Metadata, file_info: &FileInfo) {
        self.files
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        self.total_file_sizes
            .fetch_add(metadata.len(), std::sync::atomic::Ordering::Relaxed);
        self.compressed_size_start
            .fetch_add(file_info.on_disk_size, std::sync::atomic::Ordering::Relaxed);
        match file_info.compression_state {
            FileCompressionState::Compressed => {
                self.compressed_file_count_start
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
            FileCompressionState::Compressible => {}
            FileCompressionState::Incompressible(_) => {
                self.incompressible_file_count
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }
    }

    fn add_end_file(&self, _metadata: &Metadata, file_info: &FileInfo) {
        self.compressed_size_final
            .fetch_add(file_info.on_disk_size, std::sync::atomic::Ordering::Relaxed);
        if let FileCompressionState::Compressed = file_info.compression_state {
            self.compressed_file_count_final
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    #[must_use]
    pub fn compression_savings(&self) -> f64 {
        let total_file_sizes = self
            .total_file_sizes
            .load(std::sync::atomic::Ordering::Relaxed);
        let compressed_size = self
            .compressed_size_final
            .load(std::sync::atomic::Ordering::Relaxed);
        1.0 - (compressed_size as f64 / total_file_sizes as f64)
    }

    #[must_use]
    pub fn compression_change_portion(&self) -> f64 {
        let compressed_size_start = self
            .compressed_size_start
            .load(std::sync::atomic::Ordering::Relaxed);
        let compressed_size_final = self
            .compressed_size_final
            .load(std::sync::atomic::Ordering::Relaxed);
        // This is reversed because we're looking at the change in compression:
        // we want a smaller final size to be a positive change in compression
        (compressed_size_start as f64 - compressed_size_final as f64) / compressed_size_start as f64
    }
}

#[derive(Default)]
pub struct FileCompressor {
    bg_threads: BackgroundThreads,
}

impl FileCompressor {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Stage compression or decompression output in an existing directory before copying it to
    /// destination-volume temporary files, one file at a time.
    ///
    /// `limit` bounds reserved payload bytes, including work in progress.
    /// Files whose worst-case output exceeds it are left unchanged. Filesystem
    /// overhead and destination temporary files are not included in this limit.
    /// Decompression reserves each file's full uncompressed size.
    pub fn with_scratch(directory: impl AsRef<Path>, limit: u64) -> io::Result<Self> {
        Self::from_scratch(
            scratch::Scratch::new(directory.as_ref(), limit)?,
            None,
            NonZeroUsize::MIN,
        )
    }

    /// Stage compression or decompression output in process memory, then copy it
    /// to destination-volume temporary files with `publishers` parallel copy workers.
    ///
    /// Uses the same worst-case reservations as [`Self::with_scratch`]. Completed
    /// compression reservations cover buffer capacity and compression metadata.
    /// `limit` excludes worker buffers, allocator overhead, temporary allocations
    /// during buffer growth, and destination files. Buffers allocate lazily and
    /// are freed after publishing; payloads are never spilled to scratch files.
    pub fn with_memory_scratch(limit: u64, publishers: NonZeroUsize) -> io::Result<Self> {
        Self::from_scratch(scratch::Scratch::memory(limit)?, None, publishers)
    }

    /// Stage whole-file batches, then pause source reads while publishing each batch.
    ///
    /// `target` is the batch reservation target and must be positive and no greater
    /// than `limit`. A file requiring more than `target` gets its own batch if it
    /// fits `limit`. The hard limit has the same meaning as in [`Self::with_scratch`].
    /// Each batch waits for a flush of its affected destination volumes before reads resume.
    /// Verification and filesystem metadata operations may still read while publishing.
    pub fn with_scratch_batch(
        directory: impl AsRef<Path>,
        limit: u64,
        target: u64,
    ) -> io::Result<Self> {
        Self::from_scratch(
            scratch::Scratch::new(directory.as_ref(), limit)?,
            Some(target),
            NonZeroUsize::MIN,
        )
    }

    /// Stage whole-file batches in memory, then publish and flush each batch
    /// before resuming source reads.
    ///
    /// Batch behavior matches [`Self::with_scratch_batch`]; the memory limit has
    /// the same meaning as in [`Self::with_memory_scratch`]. `publishers` controls
    /// parallel destination copies; all finish before the batch's volume flush.
    pub fn with_memory_scratch_batch(
        limit: u64,
        target: u64,
        publishers: NonZeroUsize,
    ) -> io::Result<Self> {
        Self::from_scratch(scratch::Scratch::memory(limit)?, Some(target), publishers)
    }

    fn from_scratch(
        scratch: scratch::Scratch,
        target: Option<u64>,
        publishers: NonZeroUsize,
    ) -> io::Result<Self> {
        if target.is_some_and(|target| target == 0 || target > scratch.limit()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "scratch batch size must be greater than zero and no larger than the scratch limit",
            ));
        }
        Ok(Self {
            bg_threads: BackgroundThreads::with_scratch_batch(
                Some(std::sync::Arc::new(scratch)),
                target,
                publishers,
            ),
        })
    }

    #[tracing::instrument(skip_all)]
    pub fn recursive_compress<'a, P>(
        &mut self,
        paths: impl IntoIterator<Item = &'a Path>,
        encoder: impl Into<compressor::Encoder>,
        minimum_compression_ratio: f64,
        level: u32,
        progress: &P,
        verify: bool,
    ) -> Stats
    where
        P: Progress + Send + Sync,
        P::Task: Send + Sync + 'static,
    {
        self.bg_threads.scan(
            Mode::Compress {
                encoder: encoder.into(),
                level,
                minimum_compression_ratio,
            },
            paths,
            progress,
            verify,
        )
    }

    #[tracing::instrument(skip_all)]
    pub fn recursive_decompress<'a, P>(
        &mut self,
        paths: impl IntoIterator<Item = &'a Path>,
        manual: bool,
        progress: &P,
        verify: bool,
    ) -> Stats
    where
        P: Progress + Send + Sync,
        P::Task: Send + Sync + 'static,
    {
        let mode = if manual {
            Mode::DecompressManually
        } else {
            Mode::DecompressByReading
        };
        self.bg_threads.scan(mode, paths, progress, verify)
    }
}

fn try_read_all<R: Read>(mut r: R, buf: &mut [u8]) -> io::Result<usize> {
    let bulk_read_span = tracing::trace_span!(
        "try_read_all",
        len = buf.len(),
        read_len = tracing::field::Empty,
    );
    let full_len = buf.len();
    let mut remaining = buf;
    loop {
        let _enter = bulk_read_span.enter();
        let n = match r.read(remaining) {
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if n == 0 {
            break;
        }
        remaining = &mut remaining[n..];
        if remaining.is_empty() {
            return Ok(full_len);
        }
    }
    let read_len = full_len - remaining.len();

    bulk_read_span.record("read_len", read_len);
    Ok(read_len)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::progress::{SkipReason, Task};
    use crate::volumes::Volumes;
    use std::collections::HashMap;
    use std::os::unix::fs::{symlink, MetadataExt};
    use std::path::PathBuf;
    use std::sync::Mutex;
    use std::time::SystemTime;
    use std::{fs, iter};
    use tempfile::TempDir;
    use walkdir::WalkDir;

    struct NoProgress;
    impl Task for NoProgress {
        fn increment(&self, _amt: u64) {}
        fn error(&self, _message: &str) {}
    }
    impl Progress for NoProgress {
        type Task = NoProgress;

        fn error(&self, path: &Path, message: &str) {
            panic!("Expected no errors, got {message} for {path:?}");
        }

        fn file_task(&self, _path: &Path, _size: u64) -> Self::Task {
            NoProgress
        }
    }

    struct MockProgress {
        skip_reasons: Mutex<HashMap<PathBuf, SkipReason>>,
    }
    impl Progress for MockProgress {
        type Task = NoProgress;

        fn error(&self, path: &Path, message: &str) {
            panic!("Expected no errors, got {message} for {path:?}");
        }

        fn file_skipped(&self, path: &Path, why: SkipReason) {
            self.skip_reasons
                .lock()
                .unwrap()
                .insert(path.to_owned(), why);
        }

        fn file_task(&self, _path: &Path, _size: u64) -> Self::Task {
            NoProgress
        }
    }

    #[derive(Debug)]
    struct EntryInfo {
        path: PathBuf,
        modified_time: SystemTime,
        content: Option<Vec<u8>>,
    }

    fn assert_entries_equal(old: &[EntryInfo], new: &[EntryInfo]) {
        assert_eq!(old.len(), new.len());
        for (old, new) in old.iter().zip(new.iter()) {
            assert_eq!(old.path, new.path);
            assert_eq!(
                old.modified_time,
                new.modified_time,
                "modified time mismatch at {}",
                old.path.display()
            );
            assert_eq!(
                old.content,
                new.content,
                "content mismatch at {}",
                old.path.display()
            );
        }
    }

    fn recursive_read(dir: &Path) -> Vec<EntryInfo> {
        let mut result = Vec::new();
        for item in WalkDir::new(dir).sort_by_file_name() {
            let item = item.unwrap();
            let metadata = item.metadata().unwrap();
            let modified_time = metadata.modified().unwrap();
            let content = if !item.file_type().is_dir() {
                Some(fs::read(item.path()).unwrap())
            } else {
                None
            };

            result.push(EntryInfo {
                path: item.into_path(),
                modified_time,
                content,
            });
        }
        result
    }

    fn populate_dir(dir: &Path) {
        // Empty file
        fs::write(dir.join("EMPTY"), b"").unwrap();

        // Medium files
        for i in 0u8..=0xFF {
            let p = dir.join(format!("{i}"));
            fs::write(p, vec![i; usize::from(i) * 1024]).unwrap();
        }

        let subdir = dir.join("subdir");
        fs::create_dir(&subdir).unwrap();
        // Tiny Files
        for i in 0u8..=0xFF {
            let p = subdir.join(format!("{i}"));
            fs::write(p, vec![i; usize::from(i)]).unwrap();
        }

        let big_file = dir.join("BIG");
        let mut big_content = Vec::new();
        for i in 0u8..=0xFF {
            big_content.extend_from_slice(&[i; 1234]);
        }
        fs::write(big_file, big_content).unwrap();
    }

    fn compress_folder(compressor_kind: Kind, dir: &Path) {
        let mut uncompressed_file = tempfile::NamedTempFile::new().unwrap();
        uncompressed_file.write_all(&[0; 8 * 1024]).unwrap();
        uncompressed_file.flush().unwrap();
        populate_dir(dir);
        symlink(uncompressed_file.path(), dir.join("symlink")).unwrap();

        let old_contents = recursive_read(dir);

        let mut fc = FileCompressor::new();
        fc.recursive_compress(iter::once(dir), compressor_kind, 1.0, 5, &NoProgress, true);
        std::thread::sleep(std::time::Duration::from_millis(10));

        let new_contents = recursive_read(dir);
        assert_entries_equal(&old_contents, &new_contents);

        let info = info::get_recursive(dir).unwrap();
        // These are very compressible files
        assert!(info.compression_savings_fraction() > 0.5);

        // Expect symlinked file to not be compressed
        assert!(matches!(
            info::get_file_info(
                uncompressed_file.path(),
                &uncompressed_file.as_file().metadata().unwrap(),
                &Volumes::new(),
            )
            .compression_state,
            FileCompressionState::Compressible,
        ));
        assert!(dir.join("symlink").is_symlink());

        // Now Decompress
        let mut fc = FileCompressor::new();
        fc.recursive_decompress(iter::once(dir), true, &NoProgress, true);

        let new_contents = recursive_read(dir);
        assert_entries_equal(&old_contents, &new_contents);
    }

    #[test]
    fn compress_single_file() {
        let mut compressible_file = tempfile::NamedTempFile::new().unwrap();
        compressible_file.write_all(&[0; 16 * 1024]).unwrap();
        compressible_file.flush().unwrap();
        let contents = recursive_read(compressible_file.path());

        let mut fc = FileCompressor::new();
        fc.recursive_compress(
            iter::once(compressible_file.path()),
            Kind::default(),
            1.0,
            5,
            &NoProgress,
            true,
        );

        let new_contents = recursive_read(compressible_file.path());
        assert_entries_equal(&contents, &new_contents);

        let info = info::get_recursive(compressible_file.path()).unwrap();
        // These are very compressible files
        assert!(info.compression_savings_fraction() > 0.5);
    }

    #[test]
    fn compress_dir_and_file() {
        let outer_dir = TempDir::new().unwrap();
        let inner_dir = outer_dir.path().join("inner");
        fs::create_dir(&inner_dir).unwrap();
        populate_dir(&inner_dir);

        let inner_file_path = outer_dir.path().join("file");
        let mut inner_file = File::create(&inner_file_path).unwrap();
        inner_file.write_all(&[0; 16 * 1024]).unwrap();
        inner_file.flush().unwrap();

        let contents = recursive_read(outer_dir.path());

        let mut fc = FileCompressor::new();
        fc.recursive_compress(
            [inner_dir.as_path(), inner_file_path.as_path()],
            Kind::default(),
            1.0,
            5,
            &NoProgress,
            false,
        );

        let new_contents = recursive_read(outer_dir.path());
        assert_entries_equal(&contents, &new_contents);

        let info = info::get_recursive(outer_dir.path()).unwrap();
        // These are very compressible files
        assert!(info.compression_savings_fraction() > 0.5);
    }

    #[cfg(feature = "zlib")]
    #[test]
    fn compress_zlib() {
        let dir = TempDir::new().unwrap();
        compress_folder(Kind::Zlib, dir.path());
    }

    #[cfg(feature = "lzvn")]
    #[test]
    fn compress_lzvn() {
        let dir = TempDir::new().unwrap();
        compress_folder(Kind::Lzvn, dir.path());
    }

    #[cfg(feature = "lzfse")]
    #[test]
    fn compress_lzfse() {
        let dir = TempDir::new().unwrap();
        compress_folder(Kind::Lzfse, dir.path());
    }

    #[test]
    fn compress_with_hardlinks() {
        let dir = TempDir::new().unwrap();
        let orig_file = dir.path().join("test1.txt");
        fs::write(&orig_file, b"fooooooobaaaaar").unwrap();
        let second_file = dir.path().join("test2.txt");
        fs::hard_link(&orig_file, &second_file).unwrap();

        let progress = MockProgress {
            skip_reasons: Mutex::new(HashMap::new()),
        };

        let orig_contents = recursive_read(dir.path());
        let mut fc = FileCompressor::new();
        fc.recursive_compress([dir.path()], Kind::default(), 2.0, 5, &progress, false);
        let next_contents = recursive_read(dir.path());
        assert_entries_equal(&orig_contents, &next_contents);

        let skip_reasons = progress.skip_reasons.into_inner().unwrap();
        assert_eq!(skip_reasons.len(), 2);
        assert!(matches!(skip_reasons[&orig_file], SkipReason::HardLink));
        assert!(matches!(skip_reasons[&second_file], SkipReason::HardLink));

        assert!(!info::get(&orig_file).unwrap().is_compressed);
        assert!(!info::get(&second_file).unwrap().is_compressed);
    }

    #[derive(Clone, Default)]
    struct ScratchProgress {
        errors: std::sync::Arc<Mutex<Vec<String>>>,
        paths: std::sync::Arc<Mutex<Vec<PathBuf>>>,
        threshold_skips: std::sync::Arc<Mutex<Vec<PathBuf>>>,
        publishers: Option<std::sync::Arc<PublishRendezvous>>,
    }

    struct PublishRendezvous {
        expected: usize,
        threads: Mutex<std::collections::HashSet<std::thread::ThreadId>>,
        ready: std::sync::Condvar,
        timed_out: std::sync::atomic::AtomicBool,
    }

    impl PublishRendezvous {
        fn arrive(&self) {
            use std::sync::atomic::Ordering;
            let mut threads = self.threads.lock().unwrap();
            threads.insert(std::thread::current().id());
            self.ready.notify_all();
            let (_threads, result) = self
                .ready
                .wait_timeout_while(threads, std::time::Duration::from_secs(5), |threads| {
                    threads.len() < self.expected && !self.timed_out.load(Ordering::Relaxed)
                })
                .unwrap();
            if result.timed_out() {
                self.timed_out.store(true, Ordering::Relaxed);
                self.ready.notify_all();
            }
        }
    }

    impl Task for ScratchProgress {
        fn increment(&self, _amt: u64) {}
        fn error(&self, message: &str) {
            self.errors.lock().unwrap().push(message.to_owned());
        }
        fn not_compressible_enough(&self, path: &Path) {
            self.threshold_skips.lock().unwrap().push(path.to_owned());
        }
        fn phase(&self, phase: &'static str) {
            if phase == "Copying from scratch" {
                if let Some(publishers) = &self.publishers {
                    publishers.arrive();
                }
            }
        }
    }

    impl Progress for ScratchProgress {
        type Task = Self;
        fn error(&self, path: &Path, message: &str) {
            Task::error(self, &format!("{}: {message}", path.display()));
        }
        fn file_task(&self, path: &Path, _size: u64) -> Self {
            self.paths.lock().unwrap().push(path.to_owned());
            self.clone()
        }
    }

    fn scratch_compressor(
        directory: &Path,
        memory: bool,
        limit: u64,
        target: Option<u64>,
    ) -> FileCompressor {
        let publishers = NonZeroUsize::new(4).unwrap();
        match (memory, target) {
            (false, None) => FileCompressor::with_scratch(directory, limit),
            (false, Some(target)) => FileCompressor::with_scratch_batch(directory, limit, target),
            (true, None) => FileCompressor::with_memory_scratch(limit, publishers),
            (true, Some(target)) => {
                FileCompressor::with_memory_scratch_batch(limit, target, publishers)
            }
        }
        .unwrap()
    }

    #[test]
    fn memory_scratch_uses_requested_publishers_with_and_without_batches() {
        use std::sync::atomic::Ordering;
        for (publishers, batched) in [1, 4]
            .into_iter()
            .flat_map(|publishers| [false, true].map(move |batched| (publishers, batched)))
        {
            let input = TempDir::new().unwrap();
            for n in 0..32 {
                fs::write(input.path().join(format!("file-{n}")), [b'a'; 16 * 1024]).unwrap();
            }
            let before = recursive_read(input.path());
            let count = NonZeroUsize::new(publishers).unwrap();
            let limit = 8 * 1024 * 1024;
            let mut compressor = if batched {
                FileCompressor::with_memory_scratch_batch(limit, limit, count)
            } else {
                FileCompressor::with_memory_scratch(limit, count)
            }
            .unwrap();
            for compressing in [true, false] {
                let probe = std::sync::Arc::new(PublishRendezvous {
                    expected: publishers,
                    threads: Mutex::default(),
                    ready: std::sync::Condvar::new(),
                    timed_out: std::sync::atomic::AtomicBool::new(false),
                });
                let progress = ScratchProgress {
                    publishers: Some(std::sync::Arc::clone(&probe)),
                    ..ScratchProgress::default()
                };
                let stats = if compressing {
                    compressor.recursive_compress(
                        [input.path()],
                        Kind::default(),
                        0.95,
                        5,
                        &progress,
                        true,
                    )
                } else {
                    compressor.recursive_decompress([input.path()], false, &progress, true)
                };
                assert!(!probe.timed_out.load(Ordering::Relaxed));
                assert_eq!(probe.threads.lock().unwrap().len(), publishers);
                assert!(progress.errors.lock().unwrap().is_empty());
                assert_eq!(
                    stats.compressed_file_count_final.load(Ordering::Relaxed),
                    if compressing { 32 } else { 0 },
                );
                assert_entries_equal(&before, &recursive_read(input.path()));
            }
        }
    }

    #[test]
    fn scratch_round_trips_all_formats_with_bounded_backlog() {
        for (kind, memory) in [Kind::Zlib, Kind::Lzvn, Kind::Lzfse]
            .into_iter()
            .filter(|k| k.supported())
            .flat_map(|kind| [false, true].map(|memory| (kind, memory)))
        {
            let input = TempDir::new().unwrap();
            let scratch = TempDir::new().unwrap();
            fs::write(input.path().join("inline"), [b'x'; 256]).unwrap();
            // Incompressible output exercises multiple 4 MiB destination writes.
            let mut state = 0x9876_5432_1234_5678_u64;
            let data: Vec<u8> = (0..5 * 1024 * 1024 + 123)
                .map(|_| {
                    state ^= state << 13;
                    state ^= state >> 7;
                    state ^= state << 17;
                    state as u8
                })
                .collect();
            for n in 0..3 {
                fs::write(input.path().join(format!("large-{n}")), &data).unwrap();
            }
            let original_file = File::open(input.path().join("large-0")).unwrap();
            xattr::set(&original_file, c"user.applesauce-test", b"preserved", 0).unwrap();
            let before = recursive_read(input.path());
            let progress = ScratchProgress::default();
            // Only one worst-case large-file reservation fits at a time.
            let mut compressor = scratch_compressor(scratch.path(), memory, 6 * 1024 * 1024, None);
            let stats =
                compressor.recursive_compress([input.path()], kind, 1.1, 5, &progress, true);
            assert!(
                progress.errors.lock().unwrap().is_empty(),
                "{:?}",
                progress.errors.lock().unwrap()
            );
            assert_eq!(
                stats
                    .compressed_file_count_final
                    .load(std::sync::atomic::Ordering::Relaxed),
                4
            );
            assert_entries_equal(&before, &recursive_read(input.path()));
            let file = File::open(input.path().join("large-0")).unwrap();
            assert_eq!(
                xattr::read(&file, c"user.applesauce-test")
                    .unwrap()
                    .unwrap(),
                b"preserved"
            );
            for name in ["inline", "large-0"] {
                let file = File::open(input.path().join(name)).unwrap();
                let attr = xattr::read(&file, applesauce_core::decmpfs::XATTR_NAME)
                    .unwrap()
                    .unwrap();
                let value = applesauce_core::decmpfs::Value::from_data(&attr).unwrap();
                let (_, storage) = value.compression_type.compression_storage().unwrap();
                assert_eq!(
                    storage,
                    if name == "inline" {
                        applesauce_core::decmpfs::Storage::Xattr
                    } else {
                        applesauce_core::decmpfs::Storage::ResourceFork
                    }
                );
            }
            for manual in [false, true] {
                if manual {
                    compressor.recursive_compress([input.path()], kind, 1.1, 5, &progress, true);
                }
                let stats =
                    compressor.recursive_decompress([input.path()], manual, &progress, true);
                assert!(
                    progress.errors.lock().unwrap().is_empty(),
                    "{:?}",
                    progress.errors.lock().unwrap()
                );
                assert_eq!(
                    stats
                        .compressed_file_count_final
                        .load(std::sync::atomic::Ordering::Relaxed),
                    0
                );
                assert_entries_equal(&before, &recursive_read(input.path()));
                let file = File::open(input.path().join("large-0")).unwrap();
                assert_eq!(
                    xattr::read(&file, c"user.applesauce-test")
                        .unwrap()
                        .unwrap(),
                    b"preserved"
                );
                for name in ["inline", "large-0"] {
                    let file = File::open(input.path().join(name)).unwrap();
                    assert!(xattr::read(&file, applesauce_core::decmpfs::XATTR_NAME)
                        .unwrap()
                        .is_none());
                    assert!(xattr::read(&file, resource_fork::XATTR_NAME)
                        .unwrap()
                        .is_none());
                }
            }
            drop(compressor);
            assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
        }
    }

    #[cfg(feature = "lzfse")]
    #[test]
    fn lzfse_backends_round_trip_through_both_storage_pipelines() {
        use compressor::{Encoder, LzfseBackend};
        use std::sync::atomic::Ordering;
        let source = include_bytes!("threads/writer.rs");
        let data: Vec<u8> = source.iter().copied().cycle().take(160_000).collect();
        for staged in [false, true] {
            let input = TempDir::new().unwrap();
            let scratch = TempDir::new().unwrap();
            // Reuse workers while changing backend and decoding mode.
            let mut compressor = if staged {
                FileCompressor::with_scratch(scratch.path(), 1024 * 1024).unwrap()
            } else {
                FileCompressor::new()
            };
            for backend in [
                LzfseBackend::Macos,
                LzfseBackend::Crate,
                LzfseBackend::VendorUltra,
                LzfseBackend::Vendor,
            ] {
                for manual in [false, true] {
                    fs::write(input.path().join("inline"), [b'x'; 256]).unwrap();
                    fs::write(input.path().join("large"), &data).unwrap();
                    let progress = ScratchProgress::default();
                    let stats = compressor.recursive_compress(
                        [input.path()],
                        Encoder::lzfse(backend),
                        1.0,
                        5,
                        &progress,
                        true,
                    );
                    assert!(
                        progress.errors.lock().unwrap().is_empty(),
                        "{:?}",
                        progress.errors.lock().unwrap()
                    );
                    assert_eq!(stats.compressed_file_count_final.load(Ordering::Relaxed), 2);
                    assert_eq!(fs::read(input.path().join("large")).unwrap(), data);
                    for (name, expected_storage) in [
                        ("inline", applesauce_core::decmpfs::Storage::Xattr),
                        ("large", applesauce_core::decmpfs::Storage::ResourceFork),
                    ] {
                        let info = info::get(&input.path().join(name)).unwrap();
                        let decmpfs = info.decmpfs_info.unwrap().unwrap();
                        assert_eq!(
                            decmpfs.compression_type.compression_storage(),
                            Some((Kind::Lzfse, expected_storage))
                        );
                    }
                    let stats =
                        compressor.recursive_decompress([input.path()], manual, &progress, true);
                    assert!(
                        progress.errors.lock().unwrap().is_empty(),
                        "{:?}",
                        progress.errors.lock().unwrap()
                    );
                    assert_eq!(stats.compressed_file_count_final.load(Ordering::Relaxed), 0);
                    assert_eq!(fs::read(input.path().join("large")).unwrap(), data);
                    assert_eq!(fs::read(input.path().join("inline")).unwrap(), [b'x'; 256]);
                }
            }
            drop(compressor);
            assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn scratch_skips_oversized_reservations_and_its_own_directory() {
        let input = TempDir::new().unwrap();
        let small = input.path().join("small");
        let large = input.path().join("large");
        fs::write(&small, b"small contents").unwrap();
        fs::write(&large, vec![7; 256 * 1024]).unwrap();
        let progress = ScratchProgress::default();
        let mut compressor = FileCompressor::with_scratch(input.path(), 128 * 1024).unwrap();
        // Add a compressible file under the private scratch directory: it must
        // never be visited, even when scratch storage is inside the input tree.
        let private_dir = fs::read_dir(input.path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|p| p.is_dir())
            .unwrap();
        fs::write(private_dir.join("must-not-compress"), vec![0; 8192]).unwrap();
        compressor.recursive_compress([input.path()], Kind::default(), 2.0, 5, &progress, true);
        assert_eq!(progress.paths.lock().unwrap().len(), 2);
        let errors = progress.errors.lock().unwrap();
        assert_eq!(errors.len(), 1, "{errors:?}");
        assert!(errors[0].contains("scratch limit"));
        assert_eq!(fs::read(&large).unwrap(), vec![7; 256 * 1024]);
        assert!(!info::get(&large).unwrap().is_compressed);
        assert!(info::get(&small).unwrap().is_compressed);
        drop(compressor);
        assert!(!private_dir.exists());
    }

    #[test]
    fn invalid_scratch_directory_and_limit_fail_before_compression() {
        let input = TempDir::new().unwrap();
        assert!(FileCompressor::with_scratch(input.path().join("missing"), 1024).is_err());
        assert!(FileCompressor::with_scratch(input.path(), 0).is_err());
        assert!(FileCompressor::with_scratch_batch(input.path(), 1024, 0).is_err());
        assert!(FileCompressor::with_scratch_batch(input.path(), 1024, 1025).is_err());
        assert!(FileCompressor::with_memory_scratch(0, NonZeroUsize::MIN).is_err());
        assert!(FileCompressor::with_memory_scratch_batch(1024, 0, NonZeroUsize::MIN).is_err());
        assert!(FileCompressor::with_memory_scratch_batch(1024, 1025, NonZeroUsize::MIN).is_err());
        assert_eq!(fs::read_dir(input.path()).unwrap().count(), 0);
    }

    #[derive(Clone, Default)]
    struct BatchProgress {
        phases: std::sync::Arc<Mutex<Vec<(PathBuf, &'static str)>>>,
        errors: std::sync::Arc<Mutex<Vec<String>>>,
        batches: std::sync::Arc<Mutex<Vec<std::sync::Arc<BatchBytes>>>>,
    }

    struct BatchBytes {
        total: u64,
        copied: AtomicU64,
        flushing: std::sync::atomic::AtomicBool,
        finished: std::sync::atomic::AtomicBool,
    }

    struct BatchBytesTask {
        bytes: std::sync::Arc<BatchBytes>,
        progress: BatchProgress,
    }

    impl Task for BatchBytesTask {
        fn increment(&self, bytes: u64) {
            self.bytes
                .copied
                .fetch_add(bytes, std::sync::atomic::Ordering::Relaxed);
        }
        fn error(&self, message: &str) {
            panic!("{message}");
        }
        fn phase(&self, phase: &'static str) {
            if phase == "Flushing volume" {
                self.bytes
                    .flushing
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                self.progress
                    .phases
                    .lock()
                    .unwrap()
                    .push((PathBuf::new(), "Flushing scratch batch"));
            }
        }
    }

    impl Drop for BatchBytesTask {
        fn drop(&mut self) {
            self.bytes
                .finished
                .store(true, std::sync::atomic::Ordering::Relaxed);
        }
    }

    struct BatchTask {
        path: PathBuf,
        progress: BatchProgress,
    }

    impl Drop for BatchTask {
        fn drop(&mut self) {
            self.progress
                .phases
                .lock()
                .unwrap()
                .push((self.path.clone(), "Finished"));
        }
    }

    impl Task for BatchTask {
        fn increment(&self, _amt: u64) {}
        fn error(&self, message: &str) {
            self.progress
                .errors
                .lock()
                .unwrap()
                .push(message.to_owned());
        }
        fn phase(&self, phase: &'static str) {
            self.progress
                .phases
                .lock()
                .unwrap()
                .push((self.path.clone(), phase));
        }
    }

    impl Progress for BatchProgress {
        type Task = BatchTask;
        fn error(&self, path: &Path, message: &str) {
            self.errors
                .lock()
                .unwrap()
                .push(format!("{}: {message}", path.display()));
        }
        fn file_task(&self, path: &Path, _size: u64) -> Self::Task {
            BatchTask {
                path: path.to_owned(),
                progress: self.clone(),
            }
        }
        fn scratch_batch_task(&self, size: u64) -> Option<Box<dyn Task + Send + Sync>> {
            let batch = std::sync::Arc::new(BatchBytes {
                total: size,
                copied: AtomicU64::new(0),
                flushing: std::sync::atomic::AtomicBool::new(false),
                finished: std::sync::atomic::AtomicBool::new(false),
            });
            self.batches
                .lock()
                .unwrap()
                .push(std::sync::Arc::clone(&batch));
            Some(Box::new(BatchBytesTask {
                bytes: batch,
                progress: self.clone(),
            }))
        }
    }

    fn assert_batch_phases(progress: &BatchProgress, oversized: &Path, count: usize) {
        use std::collections::HashSet;
        let phases = progress.phases.lock().unwrap();
        let mut reading = HashSet::new();
        let mut staged = HashSet::new();
        let mut copying = HashSet::new();
        let mut finished = HashSet::new();
        let mut batches = 0;
        for (path, phase) in &*phases {
            match *phase {
                "Compressing to scratch" | "Decompressing to scratch" => {
                    assert!(
                        copying.is_empty(),
                        "admitted reads before batch flush: {phases:?}"
                    );
                    reading.insert(path);
                }
                "Waiting to copy" => {
                    staged.insert(path);
                }
                "Copying from scratch" => {
                    assert_eq!(
                        reading, staged,
                        "started publishing before staging completed"
                    );
                    copying.insert(path);
                }
                "Finished" => {
                    finished.insert(path);
                }
                "Flushing scratch batch" => {
                    assert_eq!(reading, copying, "flushed before copying the whole batch");
                    assert_eq!(
                        copying, finished,
                        "completed file progress kept alive during volume flush"
                    );
                    if reading.contains(&oversized.to_path_buf()) {
                        assert_eq!(reading.len(), 1, "oversized file shared a batch");
                    }
                    reading.clear();
                    staged.clear();
                    copying.clear();
                    finished.clear();
                    batches += 1;
                }
                _ => {}
            }
        }
        assert!(reading.is_empty(), "final partial batch was not published");
        assert!(batches >= 2);
        let bytes = progress.batches.lock().unwrap();
        assert_eq!(bytes.len(), batches);
        for batch in &*bytes {
            assert_eq!(
                batch.copied.load(std::sync::atomic::Ordering::Relaxed),
                batch.total
            );
            assert!(batch.flushing.load(std::sync::atomic::Ordering::Relaxed));
            assert!(batch.finished.load(std::sync::atomic::Ordering::Relaxed));
        }
        assert_eq!(
            phases
                .iter()
                .filter(|(_, phase)| *phase == "Copying from scratch")
                .count(),
            count
        );
    }

    #[test]
    fn scratch_batches_round_trip_formats_and_separate_read_write_phases() {
        for (kind, memory) in [Kind::Zlib, Kind::Lzvn, Kind::Lzfse]
            .into_iter()
            .filter(|k| k.supported())
            .flat_map(|kind| [false, true].map(|memory| (kind, memory)))
        {
            let input = TempDir::new().unwrap();
            let scratch = TempDir::new().unwrap();
            // More files than the writer and publisher queues can hold, plus an
            // oversized-but-allowed file and a final small partial batch.
            let mut paths = Vec::new();
            for n in 0..64 {
                let path = input.path().join(format!("small-{n:02}"));
                fs::write(&path, vec![n as u8; 16 * 1024]).unwrap();
                paths.push(path);
            }
            let oversized = input.path().join("oversized");
            fs::write(&oversized, vec![b'b'; 384 * 1024]).unwrap();
            paths.push(oversized.clone());
            let inline = input.path().join("inline");
            fs::write(&inline, [b'x'; 256]).unwrap();
            paths.push(inline);
            let file = File::open(&paths[0]).unwrap();
            xattr::set(&file, c"user.applesauce-test", b"preserved", 0).unwrap();
            drop(file);
            let before = recursive_read(input.path());
            let mut compressor =
                scratch_compressor(scratch.path(), memory, 512 * 1024, Some(256 * 1024));
            let progress = BatchProgress::default();
            let stats = compressor.recursive_compress(
                paths.iter().map(PathBuf::as_path),
                kind,
                0.95,
                5,
                &progress,
                true,
            );
            assert!(
                progress.errors.lock().unwrap().is_empty(),
                "{:?}",
                progress.errors.lock().unwrap()
            );
            assert_eq!(
                stats
                    .compressed_file_count_final
                    .load(std::sync::atomic::Ordering::Relaxed),
                paths.len() as u64
            );
            assert_batch_phases(&progress, &oversized, paths.len());
            let encoded_bytes: u64 = paths
                .iter()
                .map(|path| {
                    let file = File::open(path).unwrap();
                    xattr::read(&file, applesauce_core::decmpfs::XATTR_NAME)
                        .unwrap()
                        .unwrap()
                        .len() as u64
                        + resource_fork::ResourceFork::new(&file)
                            .seek(io::SeekFrom::End(0))
                            .unwrap()
                })
                .sum();
            assert_eq!(
                progress
                    .batches
                    .lock()
                    .unwrap()
                    .iter()
                    .map(|batch| batch.total)
                    .sum::<u64>(),
                encoded_bytes
            );
            assert_entries_equal(&before, &recursive_read(input.path()));
            for manual in [false, true] {
                if manual {
                    compressor.recursive_compress(
                        paths.iter().map(PathBuf::as_path),
                        kind,
                        0.95,
                        5,
                        &NoProgress,
                        true,
                    );
                }
                let progress = BatchProgress::default();
                let stats = compressor.recursive_decompress(
                    paths.iter().map(PathBuf::as_path),
                    manual,
                    &progress,
                    true,
                );
                assert!(
                    progress.errors.lock().unwrap().is_empty(),
                    "{:?}",
                    progress.errors.lock().unwrap()
                );
                assert_eq!(
                    stats
                        .compressed_file_count_final
                        .load(std::sync::atomic::Ordering::Relaxed),
                    0
                );
                assert_batch_phases(&progress, &oversized, paths.len());
                assert_eq!(
                    progress
                        .batches
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|batch| batch.total)
                        .sum::<u64>(),
                    paths
                        .iter()
                        .map(|path| path.metadata().unwrap().len())
                        .sum::<u64>()
                );
                assert_entries_equal(&before, &recursive_read(input.path()));
            }
            assert_eq!(
                xattr::read(&File::open(&paths[0]).unwrap(), c"user.applesauce-test")
                    .unwrap()
                    .unwrap(),
                b"preserved"
            );
            drop(compressor);
            assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn scratch_batch_failures_and_hard_limits_leave_originals_intact() {
        for memory in [false, true] {
            let input = TempDir::new().unwrap();
            let scratch = TempDir::new().unwrap();
            let large = input.path().join("large");
            let small = input.path().join("small");
            fs::write(&large, vec![7; 3 * 1024 * 1024]).unwrap();
            fs::write(&small, vec![b'a'; 64 * 1024]).unwrap();
            let before = recursive_read(input.path());
            let progress = ScratchProgress::default();
            // Failure on the first encoded block must wait for readers to stop,
            // release reservations, and finish even when no staged files succeeded.
            let mut compressor = scratch_compressor(
                scratch.path(),
                memory,
                4 * 1024 * 1024,
                Some(4 * 1024 * 1024),
            );
            compressor.recursive_compress([input.path()], Kind::default(), 0.0, 5, &progress, true);
            assert!(progress.errors.lock().unwrap().is_empty());
            assert_eq!(progress.threshold_skips.lock().unwrap().len(), 2);
            assert_entries_equal(&before, &recursive_read(input.path()));
            drop(compressor);
            let progress = ScratchProgress::default();
            let mut compressor =
                scratch_compressor(scratch.path(), memory, 128 * 1024, Some(32 * 1024));
            compressor.recursive_compress(
                [input.path()],
                Kind::default(),
                0.95,
                5,
                &progress,
                true,
            );
            let errors = progress.errors.lock().unwrap();
            assert_eq!(errors.len(), 1, "{errors:?}");
            assert!(errors[0].contains("scratch limit"));
            assert!(!info::get(&large).unwrap().is_compressed);
            assert!(info::get(&small).unwrap().is_compressed);
            assert_entries_equal(&before, &recursive_read(input.path()));
            drop(compressor);
            assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn scratch_threshold_skips_do_not_report_errors_without_batching() {
        for memory in [false, true] {
            let input = TempDir::new().unwrap();
            let scratch = TempDir::new().unwrap();
            let path = input.path().join("file");
            let original = vec![b'a'; 128 * 1024];
            fs::write(&path, &original).unwrap();
            let inode = path.metadata().unwrap().ino();
            let progress = ScratchProgress::default();
            let mut compressor = scratch_compressor(scratch.path(), memory, 256 * 1024, None);
            compressor.recursive_compress([input.path()], Kind::default(), 0.0, 5, &progress, true);
            assert!(progress.errors.lock().unwrap().is_empty());
            assert_eq!(
                progress.threshold_skips.lock().unwrap().as_slice(),
                std::slice::from_ref(&path)
            );
            assert_eq!(path.metadata().unwrap().ino(), inode);
            assert_eq!(fs::read(&path).unwrap(), original);
            assert!(!info::get(&path).unwrap().is_compressed);
            drop(compressor);
            assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
        }
    }

    struct FaultTask {
        path: PathBuf,
        progress: ScratchProgress,
    }

    impl Task for FaultTask {
        fn increment(&self, _amt: u64) {}
        fn error(&self, message: &str) {
            Task::error(&self.progress, message);
        }
        fn phase(&self, phase: &'static str) {
            match (self.path.file_name().unwrap().to_str().unwrap(), phase) {
                ("reader-failure", "Compressing to scratch") => {
                    fs::remove_file(&self.path).unwrap()
                }
                ("publisher-failure", "Copying from scratch") => {
                    fs::write(&self.path, b"changed after staging").unwrap()
                }
                _ => {}
            }
        }
    }

    struct FaultProgress(ScratchProgress);

    impl Progress for FaultProgress {
        type Task = FaultTask;
        fn error(&self, path: &Path, message: &str) {
            Progress::error(&self.0, path, message);
        }
        fn file_task(&self, path: &Path, _size: u64) -> Self::Task {
            FaultTask {
                path: path.to_owned(),
                progress: self.0.clone(),
            }
        }
    }

    #[test]
    fn scratch_batch_reader_and_publisher_errors_allow_later_batches() {
        for memory in [false, true] {
            let input = TempDir::new().unwrap();
            let scratch = TempDir::new().unwrap();
            let paths: Vec<_> = ["reader-failure", "publisher-failure", "success"]
                .into_iter()
                .map(|name| input.path().join(name))
                .collect();
            for path in &paths {
                fs::write(path, vec![b'a'; 128 * 1024]).unwrap();
            }
            let progress = FaultProgress(ScratchProgress::default());
            // Every file is larger than the batch target and gets its own batch.
            let mut compressor =
                scratch_compressor(scratch.path(), memory, 256 * 1024, Some(64 * 1024));
            let stats = compressor.recursive_compress(
                paths.iter().map(PathBuf::as_path),
                Kind::default(),
                0.95,
                5,
                &progress,
                true,
            );
            let errors = progress.0.errors.lock().unwrap();
            assert_eq!(errors.len(), 2, "{errors:?}");
            assert!(errors.iter().any(|e| e.contains("Error opening")));
            assert!(errors
                .iter()
                .any(|e| e.contains("source file changed after staging")));
            assert!(!paths[0].exists());
            assert_eq!(fs::read(&paths[1]).unwrap(), b"changed after staging");
            assert!(!info::get(&paths[1]).unwrap().is_compressed);
            assert!(info::get(&paths[2]).unwrap().is_compressed);
            assert_eq!(fs::read(&paths[2]).unwrap(), vec![b'a'; 128 * 1024]);
            assert_eq!(
                stats
                    .compressed_file_count_final
                    .load(std::sync::atomic::Ordering::Relaxed),
                1
            );
            drop(compressor);
            assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn scratch_decompression_reserves_uncompressed_size_with_exact_limit() {
        for (manual, memory) in [false, true]
            .into_iter()
            .flat_map(|manual| [false, true].map(|memory| (manual, memory)))
        {
            let input = TempDir::new().unwrap();
            let scratch = TempDir::new().unwrap();
            let exact = input.path().join("exact");
            let oversized = input.path().join("oversized");
            fs::write(&exact, vec![0; 128 * 1024]).unwrap();
            fs::write(&oversized, vec![0; 128 * 1024 + 1]).unwrap();
            FileCompressor::new().recursive_compress(
                [input.path()],
                Kind::default(),
                0.95,
                5,
                &NoProgress,
                true,
            );
            assert!(info::get(&exact).unwrap().is_compressed);
            assert!(info::get(&oversized).unwrap().is_compressed);
            let progress = ScratchProgress::default();
            let mut compressor = scratch_compressor(scratch.path(), memory, 128 * 1024, None);
            compressor.recursive_decompress([input.path()], manual, &progress, true);
            let errors = progress.errors.lock().unwrap();
            assert_eq!(errors.len(), 1, "{errors:?}");
            assert!(errors[0].contains("scratch limit"));
            assert!(!info::get(&exact).unwrap().is_compressed);
            assert!(info::get(&oversized).unwrap().is_compressed);
            assert_eq!(fs::read(exact).unwrap(), vec![0; 128 * 1024]);
            assert_eq!(fs::read(oversized).unwrap(), vec![0; 128 * 1024 + 1]);
            drop(compressor);
            assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
        }
    }
}
