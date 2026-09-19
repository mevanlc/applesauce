use crate::scratch::Reservation;
use crate::threads::{BgWork, Context, Mode, WorkHandler};
use crate::{seq_queue, set_flags, times, xattr};
use applesauce_core::compressor::Kind;
use applesauce_core::decmpfs;
use resource_fork::ResourceFork;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, Write};
use std::os::fd::AsRawFd;
use std::os::macos::fs::MetadataExt;
use std::sync::Arc;
use std::{cmp, io, ptr};
use tempfile::NamedTempFile;

pub(super) type Sender = crossbeam_channel::Sender<WorkItem>;

pub(super) struct Chunk {
    pub block: Vec<u8>,
    pub orig_size: u64,
}

pub(super) struct WorkItem {
    pub context: Arc<Context>,
    pub file: Arc<File>,
    pub blocks: seq_queue::Receiver<Chunk, io::Error>,
    pub reservation: Option<Reservation>,
}

pub(super) struct Work {
    pub publisher: Option<crossbeam_channel::Sender<StagedFile>>,
}

impl BgWork for Work {
    type Item = WorkItem;
    type Handler = Handler;
    const NAME: &'static str = "writer";

    fn make_handler(&self) -> Handler {
        Handler {
            decomp_xattr_val_buf: Vec::with_capacity(decmpfs::MAX_XATTR_SIZE),
            publisher: self.publisher.clone(),
        }
    }

    fn queue_capacity(&self) -> usize {
        4
    }
}

pub(super) struct Handler {
    decomp_xattr_val_buf: Vec<u8>,
    publisher: Option<crossbeam_channel::Sender<StagedFile>>,
}

impl Handler {
    #[tracing::instrument(level = "debug", skip_all, err)]
    fn write_blocks(
        &mut self,
        context: &Context,
        writer: &mut applesauce_core::writer::Writer<impl applesauce_core::writer::Open>,
        chunks: seq_queue::Receiver<Chunk, io::Error>,
    ) -> io::Result<()> {
        let block_span = tracing::debug_span!("write block");

        let mut total_compressed_size = 0;
        let minimum_compression_ratio = match context.operation.mode {
            Mode::Compress {
                minimum_compression_ratio,
                ..
            } => minimum_compression_ratio,
            _ => unreachable!("write_blocks called in non-compress mode"),
        };
        let max_compressed_size =
            (context.orig_metadata.len() as f64 * minimum_compression_ratio) as u64;

        chunks.try_for_each(|chunk| {
            total_compressed_size += u64::try_from(chunk.block.len()).unwrap();
            if total_compressed_size > max_compressed_size {
                context.progress.not_compressible_enough(&context.path);
                return Err(io::Error::other(format!(
                    "did not compress to at least {}% of original size",
                    minimum_compression_ratio * 100.0
                )));
            }

            let Chunk { block, orig_size } = chunk;
            let _enter = block_span.enter();

            writer.add_block(&block)?;
            context.progress.increment(orig_size);
            Ok(())
        })?;
        Ok(())
    }

    fn write_compressed_file(
        &mut self,
        mut item: WorkItem,
        compressor_kind: Kind,
    ) -> io::Result<()> {
        let uncompressed_file_size = item.context.orig_metadata.len();

        let tmp_file = tmp_file_for(&item)?;
        copy_xattrs(&item.file, tmp_file.as_file())?;

        let mut writer =
            applesauce_core::writer::Writer::new(compressor_kind, uncompressed_file_size, || {
                BufWriter::new(ResourceFork::new(tmp_file.as_file()))
            })?;

        self.write_blocks(&item.context, &mut writer, item.blocks)?;

        self.decomp_xattr_val_buf.clear();
        writer.finish_decmpfs_data(&mut self.decomp_xattr_val_buf)?;
        finish_compressed_file(
            &item.context,
            &mut item.file,
            tmp_file,
            &self.decomp_xattr_val_buf,
        )
    }

    fn stage_compressed_file(&mut self, mut item: WorkItem, kind: Kind) -> io::Result<()> {
        let mut reservation = item
            .reservation
            .take()
            .expect("scratch space reserved by reader");
        let scratch = item.context.operation.scratch.as_ref().unwrap();
        let payload = scratch.tempfile()?;
        let mut writer =
            applesauce_core::writer::Writer::new(kind, item.context.orig_metadata.len(), || {
                BufWriter::new(payload.as_file())
            })?;
        self.write_blocks(&item.context, &mut writer, item.blocks)?;
        let mut decmpfs_data = Vec::new();
        writer.finish_decmpfs_data(&mut decmpfs_data)?;
        reservation.shrink_to(payload.as_file().metadata()?.len() + decmpfs_data.len() as u64);
        self.send_staged(StagedFile {
            payload,
            reservation,
            file: item.file,
            contents: StagedContents::Compressed(decmpfs_data),
            context: item.context,
        })
    }

    fn stage_uncompressed_file(&mut self, mut item: WorkItem) -> io::Result<()> {
        let reservation = item
            .reservation
            .take()
            .expect("scratch space reserved by reader");
        let scratch = item.context.operation.scratch.as_ref().unwrap();
        let mut payload = scratch.tempfile()?;
        write_uncompressed_blocks(&item.context, &mut payload, item.blocks)?;
        self.send_staged(StagedFile {
            payload,
            reservation,
            file: item.file,
            contents: StagedContents::Uncompressed,
            context: item.context,
        })
    }

    fn send_staged(&self, item: StagedFile) -> io::Result<()> {
        item.context.progress.phase("Waiting to copy");
        self.publisher
            .as_ref()
            .unwrap()
            .send(item)
            .map_err(|_| io::Error::other("scratch copy worker stopped"))
    }

    fn write_uncompressed_file(&mut self, mut item: WorkItem) -> io::Result<()> {
        let mut tmp_file = tmp_file_for(&item)?;
        copy_xattrs(&item.file, tmp_file.as_file())?;
        write_uncompressed_blocks(&item.context, &mut tmp_file, item.blocks)?;
        finish_uncompressed_file(&item.context, &mut item.file, tmp_file)
    }
}

fn write_uncompressed_blocks(
    context: &Context,
    destination: &mut impl Write,
    chunks: seq_queue::Receiver<Chunk, io::Error>,
) -> io::Result<()> {
    let mut remaining = context.orig_metadata.len();
    chunks.try_for_each(|chunk| {
        let size = chunk.block.len() as u64;
        // Check before writing so malformed decoded output cannot exceed its
        // scratch reservation, even when verification is disabled.
        remaining = remaining
            .checked_sub(size)
            .ok_or_else(|| io::Error::other("decompressed data exceeds expected file size"))?;
        destination.write_all(&chunk.block)?;
        context.progress.increment(size);
        Ok(())
    })?;
    if remaining != 0 {
        return Err(io::Error::other(
            "decompressed data is shorter than expected file size",
        ));
    }
    destination.flush()
}

fn finish_uncompressed_file(
    context: &Context,
    original: &mut Arc<File>,
    mut tmp_file: NamedTempFile,
) -> io::Result<()> {
    copy_metadata(original, tmp_file.as_file())?;
    set_flags(
        tmp_file.as_file(),
        context.orig_metadata.st_flags() & !libc::UF_COMPRESSED,
    )?;
    verify_file(context, original, tmp_file.as_file_mut())?;
    persist_file(context, tmp_file)
}

fn finish_compressed_file(
    context: &Context,
    original: &mut Arc<File>,
    mut tmp_file: NamedTempFile,
    decmpfs_data: &[u8],
) -> io::Result<()> {
    {
        let _entered = tracing::debug_span!("set decmpfs xattr").entered();
        xattr::set(tmp_file.as_file(), decmpfs::XATTR_NAME, decmpfs_data, 0)?;
    }

    copy_metadata(original, tmp_file.as_file())?;
    set_flags(
        tmp_file.as_file(),
        context.orig_metadata.st_flags() | libc::UF_COMPRESSED,
    )?;

    verify_file(context, original, tmp_file.as_file_mut())?;
    persist_file(context, tmp_file)
}

fn verify_file(
    context: &Context,
    original: &mut Arc<File>,
    destination: &mut File,
) -> io::Result<()> {
    if !context.operation.verify {
        return Ok(());
    }
    context.progress.phase("Verifying");
    let _entered = tracing::info_span!("verify").entered();
    destination.rewind()?;
    let result = if matches!(context.operation.mode, Mode::DecompressManually) {
        // Manual mode must work even when the OS cannot decode this format.
        verify_manually(original, destination)
    } else {
        let orig_file = Arc::get_mut(original)
            .expect("Reader should drop file before finishing writing blocks, writer should have the only reference");
        orig_file.rewind()?;
        ensure_identical_files(BufReader::new(orig_file), BufReader::new(destination))
    };
    result.map_err(|error| {
        io::Error::other(format!(
            "verification failed: {error}, {} unchanged",
            context.path.display()
        ))
    })
}

fn verify_manually(original: &File, destination: &mut File) -> io::Result<()> {
    let mut destination = BufReader::new(destination);
    crate::rfork_storage::with_compressed_blocks(original, |kind| {
        let mut decoder = kind
            .compressor()
            .expect("reader validated compression kind");
        let mut decoded = vec![0; applesauce_core::BLOCK_SIZE + 1];
        let mut actual = vec![0; applesauce_core::BLOCK_SIZE + 1];
        let destination = &mut destination;
        move |block| {
            let len = decoder.decompress(&mut decoded, block)?;
            destination.read_exact(&mut actual[..len])?;
            if decoded[..len] != actual[..len] {
                return Err(io::Error::other("Files are not identical"));
            }
            Ok(())
        }
    })?;
    if !destination.fill_buf()?.is_empty() {
        return Err(io::Error::other("Files are not the same size"));
    }
    Ok(())
}

fn persist_file(context: &Context, tmp_file: NamedTempFile) -> io::Result<()> {
    let new_file = {
        let _entered = tracing::debug_span!("rename tmp file").entered();
        tmp_file.persist(&context.path)?
    };
    if let Some(resetter) = &context.parent_resetter {
        resetter.activate();
    }
    if let Err(e) = times::reset_times(&new_file, &context.orig_times) {
        tracing::error!("Unable to reset times: {e}");
    }
    Ok(())
}

impl WorkHandler<WorkItem> for Handler {
    fn handle_item(&mut self, item: WorkItem) {
        let context = Arc::clone(&item.context);
        let _entered = tracing::info_span!("writing file", path=%context.path.display()).entered();

        if context.operation.scratch.is_some() {
            let result = match context.operation.mode {
                Mode::Compress { kind, .. } => self.stage_compressed_file(item, kind),
                Mode::DecompressManually | Mode::DecompressByReading => {
                    self.stage_uncompressed_file(item)
                }
            };
            if let Err(error) = result {
                context
                    .progress
                    .error(&format!("{}: {error}", context.path.display()));
            }
            return;
        }

        let res = match context.operation.mode {
            Mode::Compress { kind, .. } => self.write_compressed_file(item, kind),
            Mode::DecompressManually | Mode::DecompressByReading => {
                self.write_uncompressed_file(item)
            }
        };

        match res {
            Ok(()) => {
                context.report_new_stats();
                let compressing = context.operation.mode.is_compressing();
                let prefix = if compressing { "" } else { "de" };
                tracing::info!("Successfully {prefix}compressed {}", context.path.display());
            }
            Err(error) if !context.operation.mode.is_compressing() => {
                context
                    .progress
                    .error(&format!("{}: {error}", context.path.display()));
            }
            Err(_) => {}
        }
    }
}

// Drop the payload before releasing its space reservation, and keep the context
// (which owns the scratch directory and completion notification) alive until last.
pub(super) struct StagedFile {
    payload: NamedTempFile,
    reservation: Reservation,
    file: Arc<File>,
    contents: StagedContents,
    context: Arc<Context>,
}

enum StagedContents {
    Compressed(Vec<u8>),
    Uncompressed,
}

pub(super) struct Publish;

impl BgWork for Publish {
    type Item = StagedFile;
    type Handler = PublishHandler;
    const NAME: &'static str = "scratch-copy";

    fn make_handler(&self) -> PublishHandler {
        PublishHandler {
            buffer: vec![0; 4 * 1024 * 1024],
        }
    }

    fn queue_capacity(&self) -> usize {
        4
    }
}

pub(super) struct PublishHandler {
    buffer: Vec<u8>,
}

impl PublishHandler {
    fn publish(&mut self, item: &mut StagedFile) -> io::Result<()> {
        let context = &item.context;
        let _span =
            tracing::info_span!("copying from scratch", path=%context.path.display()).entered();
        context.progress.phase("Copying from scratch");
        let mut tmp_file = context
            .operation
            .volumes
            .tempfile_for(&context.path, &context.orig_metadata)?;
        copy_xattrs(&item.file, tmp_file.as_file())?;

        // Copy the encoded resource fork explicitly. Copying a transparently
        // compressed file through its data fork would decompress it again.
        let expected = item.payload.as_file().metadata()?.len();
        item.payload.rewind()?;
        let copied = match &item.contents {
            StagedContents::Compressed(_) => copy_payload(
                item.payload.as_file_mut(),
                ResourceFork::new(tmp_file.as_file()),
                &mut self.buffer,
            )?,
            StagedContents::Uncompressed => {
                if expected != context.orig_metadata.len() {
                    return Err(io::Error::other(
                        "scratch payload has the wrong uncompressed size",
                    ));
                }
                copy_payload(
                    item.payload.as_file_mut(),
                    tmp_file.as_file_mut(),
                    &mut self.buffer,
                )?
            }
        };
        if copied != expected {
            return Err(io::Error::other(
                "scratch payload size changed while copying",
            ));
        }
        context.progress.phase("Finalizing");
        match &item.contents {
            StagedContents::Compressed(decmpfs_data) => {
                finish_compressed_file(context, &mut item.file, tmp_file, decmpfs_data)
            }
            StagedContents::Uncompressed => {
                finish_uncompressed_file(context, &mut item.file, tmp_file)
            }
        }
    }
}

impl WorkHandler<StagedFile> for PublishHandler {
    fn handle_item(&mut self, mut item: StagedFile) {
        match self.publish(&mut item) {
            Ok(()) => {
                item.context.report_new_stats();
                let prefix = if item.context.operation.mode.is_compressing() {
                    ""
                } else {
                    "de"
                };
                tracing::info!(
                    "Successfully {prefix}compressed {} via scratch",
                    item.context.path.display()
                );
            }
            Err(error) => item
                .context
                .progress
                .error(&format!("{}: {error}", item.context.path.display())),
        }
        // Space becomes available only once the scratch file is gone. Deletion
        // failure must not let reservations undercount the on-disk backlog.
        let StagedFile {
            payload,
            reservation,
            context,
            ..
        } = item;
        if let Err(error) = payload.close() {
            context
                .progress
                .error(&format!("Error removing scratch payload: {error}"));
            reservation.stop();
        }
    }
}

fn copy_payload(
    mut source: impl Read,
    mut destination: impl Write,
    buffer: &mut [u8],
) -> io::Result<u64> {
    let mut copied = 0;
    loop {
        let size = crate::try_read_all(&mut source, buffer)?;
        if size == 0 {
            break;
        }
        destination.write_all(&buffer[..size])?;
        copied += size as u64;
    }
    destination.flush()?;
    Ok(copied)
}

#[tracing::instrument(level="debug", skip_all, err, fields(path=%item.context.path.display()))]
fn tmp_file_for(item: &WorkItem) -> io::Result<NamedTempFile> {
    item.context
        .operation
        .volumes
        .tempfile_for(&item.context.path, &item.context.orig_metadata)
}

#[tracing::instrument(level = "debug", skip_all, err)]
fn copy_xattrs(src: &File, dst: &File) -> io::Result<()> {
    // SAFETY:
    //   src and dst fds are valid
    //   passing null state is allowed
    //   flags are valid
    let rc = unsafe {
        libc::fcopyfile(
            src.as_raw_fd(),
            dst.as_raw_fd(),
            ptr::null_mut(),
            libc::COPYFILE_XATTR,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[tracing::instrument(level = "debug", skip_all, err)]
fn copy_metadata(src: &File, dst: &File) -> io::Result<()> {
    // SAFETY:
    //   src and dst fds are valid
    //   passing null state is allowed
    //   flags are valid
    let rc = unsafe {
        libc::fcopyfile(
            src.as_raw_fd(),
            dst.as_raw_fd(),
            ptr::null_mut(),
            libc::COPYFILE_SECURITY,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

fn ensure_identical_files<R1: BufRead, R2: BufRead>(mut lhs: R1, mut rhs: R2) -> io::Result<()> {
    loop {
        let l = lhs.fill_buf()?;
        let r = rhs.fill_buf()?;

        if l.is_empty() && r.is_empty() {
            return Ok(());
        }
        if l.is_empty() || r.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Files are not the same size",
            ));
        }

        let min_len = cmp::min(l.len(), r.len());
        let l = &l[..min_len];
        let r = &r[..min_len];

        if l != r {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "Files are not identical",
            ));
        }

        lhs.consume(min_len);
        rhs.consume(min_len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::info;
    use crate::scratch::Scratch;
    use crate::threads::OperationContext;
    use crate::volumes::Volumes;
    use std::fs;
    use std::os::unix::fs::MetadataExt;
    use std::sync::atomic::AtomicBool;
    use tempfile::TempDir;

    struct Progress;
    impl crate::progress::Task for Progress {
        fn increment(&self, _amt: u64) {}
        fn error(&self, message: &str) {
            panic!("{message}");
        }
    }

    fn stage(input: &Path, scratch_dir: &Path) -> (StagedFile, Vec<u8>) {
        let original = vec![b'a'; 128 * 1024];
        fs::write(input, &original).unwrap();
        let metadata = input.metadata().unwrap();
        let kind = Kind::default();
        let scratch = Arc::new(Scratch::new(scratch_dir, 1024 * 1024).unwrap());
        let reservation = scratch.reserve(kind, metadata.len()).unwrap();
        let volumes = Volumes::new();
        volumes.add_root_dir(input, &metadata).unwrap();
        let orig_compression_info = info::get_file_info(input, &metadata, &volumes);
        let (finished_stats, _) = crossbeam_channel::bounded(1);
        let context = Arc::new(Context {
            parent_resetter: None,
            operation: Arc::new(OperationContext::new(
                Mode::Compress {
                    kind,
                    minimum_compression_ratio: 0.95,
                    level: 2,
                },
                finished_stats,
                volumes,
                true,
                Some(scratch),
            )),
            path: input.to_owned(),
            progress: Box::new(Progress),
            orig_metadata: metadata,
            orig_compression_info,
            orig_times: times::save_times(input).unwrap(),
            stats_reported: AtomicBool::new(false),
        });
        context
            .operation
            .stats
            .add_start_file(&context.orig_metadata, &context.orig_compression_info);
        let (tx, blocks) = seq_queue::bounded(2);
        let mut compressor = kind.compressor().unwrap();
        let mut buffer = vec![0; crate::scratch::COMPRESSED_BLOCK_CAPACITY];
        for data in original.chunks(applesauce_core::BLOCK_SIZE) {
            let len = compressor.compress(&mut buffer, data, 2).unwrap();
            tx.prepare_send()
                .unwrap()
                .finish(Chunk {
                    block: buffer[..len].to_vec(),
                    orig_size: data.len() as u64,
                })
                .unwrap();
        }
        tx.finish(Ok(()));
        let (publisher, finished) = crossbeam_channel::bounded(1);
        let mut handler = Work {
            publisher: Some(publisher),
        }
        .make_handler();
        handler
            .stage_compressed_file(
                WorkItem {
                    context,
                    file: Arc::new(File::open(input).unwrap()),
                    blocks,
                    reservation: Some(reservation),
                },
                kind,
            )
            .unwrap();
        (finished.recv().unwrap(), original)
    }

    use std::path::Path;

    #[test]
    fn scratch_publishes_only_after_copy_and_verification() {
        let input = TempDir::new().unwrap();
        let scratch = TempDir::new().unwrap();
        let path = input.path().join("file");
        let (mut staged, original) = stage(&path, scratch.path());
        let inode = path.metadata().unwrap().ino();
        assert!(!info::get(&path).unwrap().is_compressed);
        assert_eq!(fs::read(&path).unwrap(), original);
        // Scratch is an ordinary file containing the encoded resource fork.
        assert!(!info::get(staged.payload.path()).unwrap().is_compressed);
        let mut publisher = Publish.make_handler();
        publisher.publish(&mut staged).unwrap();
        assert_ne!(path.metadata().unwrap().ino(), inode);
        assert!(info::get(&path).unwrap().is_compressed);
        assert_eq!(fs::read(&path).unwrap(), original);
    }

    #[test]
    fn failed_destination_verification_leaves_original_intact() {
        let input = TempDir::new().unwrap();
        let scratch = TempDir::new().unwrap();
        let path = input.path().join("file");
        let (mut staged, mut original) = stage(&path, scratch.path());
        let inode = path.metadata().unwrap().ino();
        // A same-length edit after staging must be caught by destination verification.
        original[0] = b'b';
        fs::write(&path, &original).unwrap();
        let error = Publish.make_handler().publish(&mut staged).unwrap_err();
        assert!(error.to_string().contains("verification failed"));
        assert_eq!(path.metadata().unwrap().ino(), inode);
        assert!(!info::get(&path).unwrap().is_compressed);
        assert_eq!(fs::read(&path).unwrap(), original);
        drop(staged);
        assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
    }

    #[test]
    fn payload_copy_uses_full_chunks_and_propagates_write_failures() {
        struct Writes(Vec<usize>);
        impl Write for Writes {
            fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
                self.0.push(bytes.len());
                Ok(bytes.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let mut destination = Writes(Vec::new());
        assert_eq!(
            copy_payload(&[0; 19][..], &mut destination, &mut [0; 8]).unwrap(),
            19
        );
        assert_eq!(destination.0, [8, 8, 3]);
        let mut too_small = [0; 2];
        assert!(copy_payload(&[0; 19][..], &mut too_small[..], &mut [0; 8]).is_err());
    }

    #[test]
    fn scratch_decompression_verification_rejects_corrupted_payload() {
        for mode in [Mode::DecompressByReading, Mode::DecompressManually] {
            let input = TempDir::new().unwrap();
            let scratch = TempDir::new().unwrap();
            let path = input.path().join("file");
            let (mut staged, original) = stage(&path, scratch.path());
            Publish.make_handler().publish(&mut staged).unwrap();
            let inode = path.metadata().unwrap().ino();
            staged.file = Arc::new(File::open(&path).unwrap());
            let context = Arc::get_mut(&mut staged.context).unwrap();
            context.orig_metadata = path.metadata().unwrap();
            context.orig_compression_info =
                info::get_file_info(&path, &context.orig_metadata, &context.operation.volumes);
            Arc::get_mut(&mut context.operation).unwrap().mode = mode;
            staged.reservation = context
                .operation
                .scratch
                .as_ref()
                .unwrap()
                .reserve_uncompressed(original.len() as u64)
                .unwrap();
            staged.payload.as_file_mut().set_len(0).unwrap();
            staged.payload.rewind().unwrap();
            staged.payload.write_all(&original).unwrap();
            staged.payload.rewind().unwrap();
            staged.payload.write_all(b"b").unwrap();
            staged.contents = StagedContents::Uncompressed;
            let error = Publish.make_handler().publish(&mut staged).unwrap_err();
            assert!(error.to_string().contains("verification failed"), "{error}");
            assert_eq!(path.metadata().unwrap().ino(), inode);
            assert!(info::get(&path).unwrap().is_compressed);
            assert_eq!(fs::read(&path).unwrap(), original);
            drop(staged);
            assert_eq!(fs::read_dir(scratch.path()).unwrap().count(), 0);
        }
    }

    #[test]
    fn uncompressed_output_must_match_its_reserved_size() {
        let input = TempDir::new().unwrap();
        let scratch = TempDir::new().unwrap();
        let (staged, original) = stage(&input.path().join("file"), scratch.path());
        for size in [original.len() + 1, original.len() - 1] {
            let (tx, rx) = seq_queue::bounded(1);
            tx.prepare_send()
                .unwrap()
                .finish(Chunk {
                    block: vec![0; size],
                    orig_size: 1,
                })
                .unwrap();
            tx.finish(Ok(()));
            let mut output = Vec::new();
            assert!(write_uncompressed_blocks(&staged.context, &mut output, rx).is_err());
            if size > original.len() {
                assert!(
                    output.is_empty(),
                    "oversized block must be rejected before writing"
                );
            }
        }
    }
}
