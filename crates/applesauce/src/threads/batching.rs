use super::{reader, writer::StagedFile, Context, Mode};
use crate::progress::{Progress, Task};
use crate::scratch::Scratch;
use crossbeam_channel::{Receiver, Sender};
use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Coordinates admission on the scanning thread. Completed files stay in scratch
/// until every admitted reader and staging writer has finished, including failures.
pub(super) struct Batch<'a, P> {
    target: u64,
    scratch: Arc<Scratch>,
    publisher: Sender<StagedFile>,
    completed_tx: Sender<Option<StagedFile>>,
    completed_rx: Receiver<Option<StagedFile>>,
    pending: usize,
    progress: &'a P,
}

impl<'a, P: Progress> Batch<'a, P> {
    pub(super) fn new(
        target: u64,
        scratch: Arc<Scratch>,
        publisher: Sender<StagedFile>,
        progress: &'a P,
    ) -> Self {
        // Notifications cannot block staging workers: the coordinator receives them
        // only once it stops admitting files. Payloads remain on disk, not in RAM.
        let (completed_tx, completed_rx) = crossbeam_channel::unbounded();
        Self {
            target,
            scratch,
            publisher,
            completed_tx,
            completed_rx,
            pending: 0,
            progress,
        }
    }

    pub(super) fn submit(&mut self, context: Arc<Context>, reader: &Sender<reader::WorkItem>) {
        let size = context.orig_metadata.len();
        let bytes = match context.operation.mode {
            Mode::Compress { encoder, .. } => {
                Scratch::compressed_reservation_size(encoder.kind(), size)
            }
            Mode::DecompressManually | Mode::DecompressByReading => size,
        };
        if bytes <= self.scratch.limit()
            && self.pending > 0
            && bytes > self.target.saturating_sub(self.scratch.reserved_bytes())
        {
            self.drain();
        }
        let reservation = match self.scratch.reserve_bytes(bytes) {
            Ok(reservation) => reservation,
            Err(error) => {
                context
                    .progress
                    .error(&format!("{}: {error}", context.path.display()));
                return;
            }
        };
        self.pending += 1;
        reader
            .send(reader::WorkItem {
                context,
                reservation: Some(reservation),
                completion: Some(Completion::new(self.completed_tx.clone())),
            })
            .unwrap();
        if bytes > self.target {
            // Keep an oversized-but-allowed file alone even if its reservation
            // shrinks below the target after compression.
            self.drain();
        }
    }

    pub(super) fn drain(&mut self) {
        if self.pending == 0 {
            return;
        }
        let _batch_span = tracing::info_span!("scratch batch", files = self.pending).entered();
        let staged: Vec<_> = {
            let _wait = tracing::info_span!("waiting for batch staging").entered();
            (0..self.pending)
                .filter_map(|_| self.completed_rx.recv().expect("staging completion guard"))
                .collect()
        };
        let count = staged.len();
        let batch_progress: Option<Arc<dyn Task + Send + Sync>> = if count == 0 {
            None
        } else {
            self.progress
                .scratch_batch_task(staged.iter().map(StagedFile::publish_size).sum())
                .map(Arc::from)
        };
        let _publish = tracing::info_span!("publishing scratch batch", files = count).entered();
        // An unbounded acknowledgement channel lets the bounded publisher queue
        // drain while the coordinator is still sending the rest of the batch.
        let (finished_tx, finished_rx) = crossbeam_channel::unbounded();
        let mut volumes = HashMap::new();
        for mut file in staged {
            // Retain only a path for the volume flush so completed file tasks,
            // including their progress bars, can end immediately after publishing.
            volumes
                .entry(file.context.orig_metadata.dev())
                .or_insert_with(|| file.context.path.clone());
            file.finished = Some(finished_tx.clone());
            file.batch_progress = batch_progress.as_ref().map(Arc::clone);
            self.publisher.send(file).unwrap();
        }
        drop(finished_tx);
        for _ in 0..count {
            finished_rx.recv().expect("publisher completion");
        }
        // Waiting for copies alone would let macOS buffered writeback overlap the
        // next read phase. Flush each affected destination volume once per batch.
        if let Some(progress) = &batch_progress {
            progress.phase("Flushing volume");
        }
        for path in volumes.into_values() {
            if let Err(error) = flush_volume(&path) {
                self.progress
                    .error(&path, &format!("Error flushing scratch batch: {error}"));
                self.scratch.stop();
            }
        }
        self.pending = 0;
    }
}

fn flush_volume(path: &Path) -> io::Result<()> {
    unsafe extern "C" {
        fn fsync_volume_np(fd: libc::c_int, flags: libc::c_int) -> libc::c_int;
    }
    // Values from the macOS SDK's unistd.h. libc does not expose this API.
    const SYNC_VOLUME_FULLSYNC: libc::c_int = 0x01;
    const SYNC_VOLUME_WAIT: libc::c_int = 0x02;
    // The source may have disappeared on a failed publish. Its parent directory
    // still identifies the volume whose buffered writes need to finish.
    let directory = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let file = File::open(directory)?;
    let _flush =
        tracing::info_span!("flushing scratch batch volume", path = %path.display()).entered();
    // SAFETY: the file descriptor is valid for this call and flags are SDK-defined.
    let result =
        unsafe { fsync_volume_np(file.as_raw_fd(), SYNC_VOLUME_FULLSYNC | SYNC_VOLUME_WAIT) };
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Both the reader and writer hold this guard. In particular, an early staging
/// failure must not release the barrier while the reader is still reading a block.
#[derive(Clone)]
pub(super) struct Completion(Arc<CompletionState>);

struct CompletionState {
    staged: Mutex<Option<StagedFile>>,
    completed: Sender<Option<StagedFile>>,
}

impl Completion {
    fn new(completed: Sender<Option<StagedFile>>) -> Self {
        Self(Arc::new(CompletionState {
            staged: Mutex::new(None),
            completed,
        }))
    }

    pub(super) fn finish(self, staged: StagedFile) {
        *self.0.staged.lock().unwrap() = Some(staged);
    }
}

impl Drop for CompletionState {
    fn drop(&mut self) {
        let _ = self.completed.send(self.staged.get_mut().unwrap().take());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_staging_waits_for_reader_before_releasing_barrier() {
        let (tx, rx) = crossbeam_channel::unbounded();
        let reader = Completion::new(tx);
        let writer = reader.clone();
        drop(writer);
        assert!(rx.try_recv().is_err());
        drop(reader);
        assert!(rx.recv().unwrap().is_none());
        assert!(rx.try_recv().is_err());
    }
}
