//! Native bridge: runs the async core on its own tokio thread and exposes a non-blocking
//! command/event interface to the immediate-mode UI.

use std::{
    path::PathBuf,
    sync::{Mutex, mpsc as std_mpsc},
    time::Duration,
};

use brumm_core::{
    api::{Command, Event, EventSink},
    node::{self, NodeConfig, SHUTDOWN_DRAIN_MAX},
};
use brumm_host::ProfileLock;
use tokio::sync::mpsc;

/// The core is network-light: a small multi-thread runtime is enough and keeps
/// idle thread/stack overhead low.
const CORE_WORKER_THREADS: usize = 2;
/// Longest the exiting process waits for the core's shutdown drain (it is capped there too).
const EXIT_WAIT: Duration = Duration::from_secs(SHUTDOWN_DRAIN_MAX.as_secs() + 3);

/// Handed over by `CoreBridge::drop`, so the wait happens in `wait_for_drain` after the window
/// is gone and not while it is still on screen.
static DRAIN_SIGNAL: Mutex<Option<std_mpsc::Receiver<()>>> = Mutex::new(None);

/// Blocks until the core has put unsent messages on the relays (a second or so, capped).
/// Call after the UI loop ended; returns at once if nothing is pending.
pub fn wait_for_drain() {
    let signal = DRAIN_SIGNAL.lock().ok().and_then(|mut slot| slot.take());
    if let Some(signal) = signal {
        let _ = signal.recv_timeout(EXIT_WAIT);
    }
}

pub struct CoreBridge {
    commands: Option<mpsc::UnboundedSender<Command>>,
    events: mpsc::UnboundedReceiver<Event>,
    /// Signalled when the core's shutdown drain is over (see [`wait_for_drain`]).
    drained: Option<std_mpsc::Receiver<()>>,
}

impl CoreBridge {
    pub fn start(data_dir: PathBuf, ctx: egui::Context) -> Self {
        let (command_tx, command_rx) = mpsc::unbounded_channel();
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let sink = EventSink::new(event_tx, move || ctx.request_repaint());
        let (done_tx, drained) = std_mpsc::channel();
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
                let mut config = NodeConfig::new(data_dir);
                let drained_tx = done_tx.clone();
                config.on_drained = Some(Box::new(move || {
                    let _ = drained_tx.send(());
                }));
                runtime.block_on(node::run(config, command_rx, sink));
                let _ = done_tx.send(());
            })
            .expect("spawn core thread");
        Self {
            commands: Some(command_tx),
            events: event_rx,
            drained: Some(drained),
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
    /// Nostr relays; the process waits for that in [`wait_for_drain`], not here (the window
    /// would linger), and not for the graceful network teardown after it.
    fn drop(&mut self) {
        self.commands.take();
        if let (Some(drained), Ok(mut slot)) = (self.drained.take(), DRAIN_SIGNAL.lock()) {
            *slot = Some(drained);
        }
    }
}
