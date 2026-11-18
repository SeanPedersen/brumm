//! Runs the async core on its own tokio thread and exposes a non-blocking command/event
//! interface to the immediate-mode UI.

use std::{path::PathBuf, thread::JoinHandle};

use brumm_core::{
    api::{Command, Event, EventSink},
    node::{self, NodeConfig},
};
use tokio::sync::mpsc;

pub struct CoreBridge {
    commands: Option<mpsc::UnboundedSender<Command>>,
    events: mpsc::UnboundedReceiver<Event>,
    thread: Option<JoinHandle<()>>,
}

impl CoreBridge {
    pub fn start(data_dir: PathBuf, ctx: egui::Context) -> Self {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let sink = EventSink::new(event_tx, move || ctx.request_repaint());
        let thread = std::thread::Builder::new()
            .name("brumm-core".into())
            .spawn(move || {
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                runtime.block_on(node::run(NodeConfig { data_dir }, command_rx, sink));
            })
            .expect("spawn core thread");
        Self {
            commands: Some(command_tx),
            events: event_rx,
            thread: Some(thread),
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
    /// Closing the command channel stops the node, which then shuts iroh down gracefully.
    fn drop(&mut self) {
        self.commands.take();
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}
