//! Native bridge: runs the async core on its own tokio thread and exposes a non-blocking
//! command/event interface to the immediate-mode UI.

use std::{path::PathBuf, sync::mpsc as std_mpsc, time::Duration};

use brumm_core::{
    api::{Command, Event, EventSink},
    node::{self, NodeConfig, SHUTDOWN_DRAIN_MAX},
};
use tokio::sync::mpsc;

use super::profile_lock::ProfileLock;

/// The core is network-light: a small multi-thread runtime is enough and keeps
/// idle thread/stack overhead low.
const CORE_WORKER_THREADS: usize = 2;
/// Longest the exiting process waits for the core's shutdown drain (it is capped there too).
const EXIT_WAIT: Duration = Duration::from_secs(SHUTDOWN_DRAIN_MAX.as_secs() + 3);

pub struct CoreBridge {
    commands: Option<mpsc::UnboundedSender<Command>>,
    events: mpsc::UnboundedReceiver<Event>,
    /// Signalled when the core thread has finished, shutdown drain included.
    core_done: std_mpsc::Receiver<()>,
}

impl CoreBridge {
    pub fn start(data_dir: PathBuf, ctx: egui::Context) -> Self {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let sink = EventSink::new(event_tx, move || ctx.request_repaint());
        let (done_tx, core_done) = std_mpsc::channel();
        let failures = sink.clone();
        // The core stops when the command channel closes; `Drop` then waits for its drain.
        std::thread::Builder::new()
            .name("brumm-core".into())
            .spawn(move || {
                // Held until the core is completely done, so a relaunch cannot overlap the drain.
                let _lock = match ProfileLock::acquire(&data_dir) {
                    Ok(lock) => lock,
                    Err(err) => {
                        failures.emit(Event::Error(format!("{err:#}")));
                        let _ = done_tx.send(());
                        return;
                    }
                };
                let runtime = tokio::runtime::Builder::new_multi_thread()
                    .worker_threads(CORE_WORKER_THREADS)
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                runtime.block_on(node::run(NodeConfig::new(data_dir), command_rx, sink));
                let _ = done_tx.send(());
            })
            .expect("spawn core thread");
        Self {
            commands: Some(command_tx),
            events: event_rx,
            core_done,
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
    /// Closing the command channel asks the node to stop. It first puts unsent messages on the
    /// Nostr relays (a second or so), so the process waits for that, bounded by [`EXIT_WAIT`].
    fn drop(&mut self) {
        self.commands.take();
        let _ = self.core_done.recv_timeout(EXIT_WAIT);
    }
}
