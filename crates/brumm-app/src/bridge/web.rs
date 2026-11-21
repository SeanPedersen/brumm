//! Browser bridge: starts the brumm-worker Web Worker and exchanges postcard-encoded
//! [`ToWorker`] / [`FromWorker`] messages with it.
//!
//! A worker drops messages posted before its wasm module has installed a handler, so commands
//! are queued until the worker reports [`FromWorker::Listening`]; `Start` always goes first.

use std::{cell::RefCell, rc::Rc};

use brumm_core::api::{Command, Event, FromWorker, ToWorker};
use tokio::sync::mpsc;
use wasm_bindgen::{JsCast, prelude::Closure};
use web_sys::{MessageEvent, Worker};

/// Loader script trunk emits for the `brumm-worker` crate (see index.html).
const WORKER_SCRIPT: &str = "./brumm-worker_loader.js";

pub struct CoreBridge {
    link: Rc<RefCell<WorkerLink>>,
    events: mpsc::UnboundedReceiver<Event>,
    /// Keeps the message handler alive as long as the bridge.
    _on_message: Closure<dyn FnMut(MessageEvent)>,
}

struct WorkerLink {
    worker: Worker,
    listening: bool,
    queued: Vec<ToWorker>,
}

impl WorkerLink {
    fn post(&mut self, message: ToWorker) {
        if !self.listening {
            self.queued.push(message);
            return;
        }
        post(&self.worker, &message);
    }

    fn open(&mut self) {
        self.listening = true;
        for message in std::mem::take(&mut self.queued) {
            post(&self.worker, &message);
        }
    }
}

impl CoreBridge {
    pub fn start(profile: String, ctx: egui::Context) -> Self {
        let worker = Worker::new(WORKER_SCRIPT).expect("start the brumm-worker Web Worker");
        let link = Rc::new(RefCell::new(WorkerLink {
            worker: worker.clone(),
            listening: false,
            queued: vec![ToWorker::Start { profile }],
        }));
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let handler_link = link.clone();
        let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |message: MessageEvent| {
            let bytes = js_sys::Uint8Array::new(&message.data()).to_vec();
            match postcard::from_bytes::<FromWorker>(&bytes) {
                Ok(FromWorker::Listening) => handler_link.borrow_mut().open(),
                Ok(FromWorker::Event(event)) => {
                    let _ = event_tx.send(event);
                    ctx.request_repaint();
                }
                Err(err) => tracing::warn!(?err, "undecodable message from the core worker"),
            }
        });
        worker.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
        Self {
            link,
            events: event_rx,
            _on_message: on_message,
        }
    }

    pub fn send(&self, command: Command) {
        self.link.borrow_mut().post(ToWorker::Command(command));
    }

    pub fn drain(&mut self) -> impl Iterator<Item = Event> + '_ {
        std::iter::from_fn(|| self.events.try_recv().ok())
    }
}

impl Drop for CoreBridge {
    fn drop(&mut self) {
        self.link.borrow().worker.terminate();
    }
}

fn post(worker: &Worker, message: &ToWorker) {
    let bytes = match postcard::to_allocvec(message) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(?err, "could not encode a message for the core worker");
            return;
        }
    };
    let array = js_sys::Uint8Array::from(bytes.as_slice());
    let transfer = js_sys::Array::of1(&array.buffer());
    if let Err(err) = worker.post_message_with_transfer(&array, &transfer) {
        tracing::warn!(?err, "postMessage to the core worker failed");
    }
}
