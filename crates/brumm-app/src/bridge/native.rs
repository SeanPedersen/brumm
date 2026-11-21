//! Native bridge: runs the async core on its own tokio thread and exposes a non-blocking
//! command/event interface to the immediate-mode UI.

use std::path::PathBuf;

use brumm_core::{
    api::{Command, Event, EventSink},
    node::{self, NodeConfig},
};
use tokio::sync::mpsc;

/// The core is network-light: a small multi-thread runtime is enough and keeps
/// idle thread/stack overhead low.
const CORE_WORKER_THREADS: usize = 2;

pub struct CoreBridge {
    commands: Option<mpsc::UnboundedSender<Command>>,
    events: mpsc::UnboundedReceiver<Event>,
}

impl CoreBridge {
    pub fn start(data_dir: PathBuf, ctx: egui::Context) -> Self {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let sink = EventSink::new(event_tx, move || ctx.request_repaint());
        // Detached: the core stops when the command channel closes, but shutdown never
        // blocks the window from closing (see `Drop`).
        std::thread::Builder::new()
            .name("brumm-core".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(CORE_WORKER_THREADS)
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                runtime.block_on(node::run(NodeConfig::new(data_dir), command_rx, sink));
            })
            .expect("spawn core thread");
        Self {
            commands: Some(command_tx),
            events: event_rx,
        }
    }

    pub fn send(&self, command: Command) {
        if let Some(commands) = &self.commands {
            let _ = commands.send(command);
        }
    }

    pub fn drain(&mut self) -> impl Iterator<Item = Event> + '_ {
        std::iter::from_fn(|| self.events.try_recv().ok())
    }
}

impl Drop for CoreBridge {
    /// Closing the command channel asks the node to stop. Do not wait for iroh/Nostr
    /// teardown: the process is exiting, and waiting here makes the window linger.
    fn drop(&mut self) {
        self.commands.take();
    }
}
