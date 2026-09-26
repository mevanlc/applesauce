use crate::scratch::COMPRESSED_BLOCK_CAPACITY;
use crate::seq_queue;
use crate::threads::{writer, BgWork, Context, Mode, WorkHandler};
use applesauce_core::compressor::{self, Compressor};
use std::collections::HashMap;
use std::io;
use std::sync::Arc;

pub(super) type Sender = crossbeam_channel::Sender<WorkItem>;

pub(super) struct WorkItem {
    pub context: Arc<Context>,
    pub data: Vec<u8>,
    pub kind: compressor::Kind,
    pub slot: seq_queue::Slot<writer::Chunk, io::Error>,
}

pub(super) struct Work;

impl BgWork for Work {
    type Item = WorkItem;
    type Handler = Handler;
    const NAME: &'static str = "compressor";

    fn make_handler(&self) -> Self::Handler {
        Handler {
            compressors: HashMap::new(),
            buf: vec![0; COMPRESSED_BLOCK_CAPACITY],
        }
    }

    fn queue_capacity(&self) -> usize {
        8
    }
}

pub(super) struct Handler {
    compressors: HashMap<compressor::Encoder, Compressor>,
    buf: Vec<u8>,
}

impl WorkHandler<WorkItem> for Handler {
    fn handle_item(&mut self, item: WorkItem) {
        let _entered =
            tracing::debug_span!("compressing block", path=%item.context.path.display()).entered();

        let encoder = match item.context.operation.mode {
            Mode::Compress { encoder, .. } => encoder,
            _ => item.kind.into(),
        };
        let compressor = self
            .compressors
            .entry(encoder)
            .or_insert_with(|| encoder.compressor().expect("supported encoder"));
        let size = match item.context.operation.mode {
            Mode::Compress { encoder, level, .. } => {
                debug_assert_eq!(encoder.kind(), item.kind);
                compressor.compress(&mut self.buf, &item.data, level)
            }
            Mode::DecompressManually => compressor.decompress(&mut self.buf, &item.data),
            Mode::DecompressByReading => {
                panic!("decompressing by reading should not be using the compressor thread")
            }
        };
        let size = match size {
            Ok(size) => size,
            Err(e) => {
                item.slot.error(e);
                return;
            }
        };
        debug_assert!(size != 0);

        let chunk = writer::Chunk {
            block: self.buf[..size].to_vec(),
            orig_size: item.data.len().try_into().unwrap(),
        };
        if item.slot.finish(chunk).is_err() {
            // This should only be because of a failure already reported by the writer
            tracing::debug!("unable to finish slot");
        }
    }
}
