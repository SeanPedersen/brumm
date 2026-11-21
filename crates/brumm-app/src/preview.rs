//! Decodes chat photos on one background thread so a large JPEG never stalls the UI.
//!
//! The thread scales the photo down to the bubble (`prepare_chat_image`) and hands back only
//! the preview bytes. The UI thread installs that result and asks the core to store it.
//! The browser build has no threads here: it decodes on submit, and the stored preview makes
//! that a one-time cost per photo.

use std::sync::Arc;
#[cfg(not(target_family = "wasm"))]
use std::sync::mpsc::{self, Receiver, Sender};

use brumm_core::{identity::UserKey, wire::MessageId};

use crate::{
    state::LoadedAttachment,
    ui::files::{self, ChatImage},
};

/// Freed allocations at least this large are worth returning to the OS. A 4K decode is much bigger.
const HEAP_TRIM_BYTES: usize = 64 * 1024;

struct Job {
    contact: UserKey,
    id: MessageId,
    bytes: Arc<Vec<u8>>,
    max_edge: u32,
    /// Repainted when the background thread finishes; the browser decodes inside the frame.
    #[cfg_attr(target_family = "wasm", allow(dead_code))]
    ctx: egui::Context,
}

/// A preview finished off the UI thread.
pub struct ReadyPreview {
    pub contact: UserKey,
    pub id: MessageId,
    pub loaded: LoadedAttachment,
    /// Encoded preview to persist. Absent when the original is already bubble-sized, or not an image.
    pub persist: Option<Vec<u8>>,
    pub source_size: Option<(u32, u32)>,
    pub max_edge: u32,
}

#[cfg(not(target_family = "wasm"))]
pub struct PreviewLoader {
    jobs: Sender<Job>,
    ready: Receiver<ReadyPreview>,
}

#[cfg(target_family = "wasm")]
pub struct PreviewLoader {
    jobs: std::cell::RefCell<Vec<Job>>,
}

impl PreviewLoader {
    #[cfg(not(target_family = "wasm"))]
    pub fn start() -> Self {
        let (job_tx, job_rx) = mpsc::channel::<Job>();
        let (ready_tx, ready_rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("brumm-preview".into())
            .spawn(move || {
                while let Ok(job) = job_rx.recv() {
                    let ctx = job.ctx.clone();
                    let ready = decode(job);
                    if ready_tx.send(ready).is_err() {
                        break;
                    }
                    ctx.request_repaint();
                }
            })
            .expect("preview thread");
        Self {
            jobs: job_tx,
            ready: ready_rx,
        }
    }

    /// Jobs are decoded in [`Self::poll`], on the UI thread.
    #[cfg(target_family = "wasm")]
    pub fn start() -> Self {
        Self {
            jobs: std::cell::RefCell::default(),
        }
    }

    pub fn submit(
        &self,
        contact: UserKey,
        id: MessageId,
        bytes: Arc<Vec<u8>>,
        max_edge: u32,
        ctx: egui::Context,
    ) {
        let job = Job {
            contact,
            id,
            bytes,
            max_edge,
            ctx,
        };
        #[cfg(not(target_family = "wasm"))]
        let _ = self.jobs.send(job);
        #[cfg(target_family = "wasm")]
        self.jobs.borrow_mut().push(job);
    }

    #[cfg(not(target_family = "wasm"))]
    pub fn poll(&self) -> impl Iterator<Item = ReadyPreview> + '_ {
        std::iter::from_fn(|| self.ready.try_recv().ok())
    }

    #[cfg(target_family = "wasm")]
    pub fn poll(&self) -> impl Iterator<Item = ReadyPreview> + '_ {
        let jobs = std::mem::take(&mut *self.jobs.borrow_mut());
        jobs.into_iter().map(decode)
    }
}

fn decode(job: Job) -> ReadyPreview {
    let source_len = job.bytes.len();
    let started = web_time::Instant::now();
    let prepared = files::prepare_chat_image(job.bytes.as_slice(), job.max_edge);
    let (loaded, persist, source_size) = match prepared {
        Some(ChatImage {
            size,
            preview: Some(preview),
        }) => {
            let persist = Some(preview.clone());
            (
                LoadedAttachment {
                    bytes: Arc::from(preview),
                    image_size: Some(size),
                },
                persist,
                Some(size),
            )
        }
        Some(ChatImage {
            size,
            preview: None,
        }) => (
            LoadedAttachment {
                bytes: arc_bytes(job.bytes),
                image_size: Some(size),
            },
            None,
            Some(size),
        ),
        None => (
            LoadedAttachment {
                bytes: Arc::from([]),
                image_size: None,
            },
            None,
            None,
        ),
    };
    if persist.is_some() && source_len >= HEAP_TRIM_BYTES {
        files::release_freed_heap();
    }
    tracing::debug!(
        ms = started.elapsed().as_millis(),
        bytes = source_len,
        stored = persist.is_some(),
        "chat preview ready"
    );
    ReadyPreview {
        contact: job.contact,
        id: job.id,
        loaded,
        persist,
        source_size,
        max_edge: job.max_edge,
    }
}

fn arc_bytes(data: Arc<Vec<u8>>) -> Arc<[u8]> {
    match Arc::try_unwrap(data) {
        Ok(bytes) => Arc::from(bytes),
        Err(bytes) => Arc::from(&bytes[..]),
    }
}
