//! Worker side of the browser bridge: storage setup, node start, and message pumping.

use std::path::PathBuf;

use brumm_core::{
    api::{Command, EventSink, FromWorker, ToWorker},
    node::{self, NodeConfig},
};
use sqlite_wasm_vfs::sahpool::{self, OpfsSAHPoolCfgBuilder};
use tokio::sync::mpsc;
use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};
use wasm_bindgen::{JsCast, prelude::Closure};
use web_sys::{DedicatedWorkerGlobalScope, MessageEvent};

const LOG_FILTER: &str = "brumm_core=info,brumm_worker=info,warn";
/// SQLite path inside the profile's pool; the pool directory already separates profiles.
const DATA_DIR: &str = "/";
const OPFS_ROOT: &str = "brumm/profiles";

brumm_core::register_browser_clock!();

pub fn main() {
    console_error_panic_hook::set_once();
    init_tracing();

    let scope: DedicatedWorkerGlobalScope = js_sys::global().unchecked_into();
    let (command_tx, command_rx) = mpsc::unbounded_channel::<Command>();
    let mut command_rx = Some(command_rx);
    let poster = scope.clone();

    let on_message = Closure::<dyn FnMut(MessageEvent)>::new(move |message: MessageEvent| {
        let bytes = js_sys::Uint8Array::new(&message.data()).to_vec();
        match postcard::from_bytes::<ToWorker>(&bytes) {
            Ok(ToWorker::Command(command)) => {
                let _ = command_tx.send(command);
            }
            Ok(ToWorker::Start { profile }) => {
                let Some(commands) = command_rx.take() else {
                    tracing::warn!("ignoring a second Start");
                    return;
                };
                wasm_bindgen_futures::spawn_local(run(profile, commands, poster.clone()));
            }
            Err(err) => tracing::warn!(?err, "undecodable message from the UI"),
        }
    });
    scope.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    // The worker lives as long as the page; the handler must too.
    on_message.forget();
    post(&scope, &FromWorker::Listening);
}

async fn run(
    profile: String,
    commands: mpsc::UnboundedReceiver<Command>,
    scope: DedicatedWorkerGlobalScope,
) {
    let (event_tx, mut event_rx) = mpsc::unbounded_channel();
    // The UI repaints when a message arrives on its side; nothing to wake here.
    let events = EventSink::new(event_tx, || {});
    let forward_to = scope.clone();
    wasm_bindgen_futures::spawn_local(async move {
        while let Some(event) = event_rx.recv().await {
            post(&forward_to, &FromWorker::Event(event));
        }
    });

    if let Err(err) = install_storage(&profile).await {
        events.emit(brumm_core::api::Event::Error(format!(
            "browser storage unavailable (is brumm already open in another tab?): {err:#}"
        )));
        return;
    }
    tracing::info!(profile, "starting brumm core in worker");
    node::run(NodeConfig::new(PathBuf::from(DATA_DIR)), commands, events).await;
}

/// Makes an OPFS pool for this profile SQLite's default VFS, so `rusqlite` opens it by path.
async fn install_storage(profile: &str) -> anyhow::Result<()> {
    let config = OpfsSAHPoolCfgBuilder::new()
        .directory(&format!("{OPFS_ROOT}/{profile}"))
        .build();
    sahpool::install::<sqlite_wasm_rs::WasmOsCallback>(&config, true)
        .await
        .map_err(|err| anyhow::anyhow!("{err:?}"))?;
    Ok(())
}

fn post(scope: &DedicatedWorkerGlobalScope, message: &FromWorker) {
    let bytes = match postcard::to_allocvec(message) {
        Ok(bytes) => bytes,
        Err(err) => {
            tracing::warn!(?err, "could not encode a message for the UI");
            return;
        }
    };
    let array = js_sys::Uint8Array::from(bytes.as_slice());
    let transfer = js_sys::Array::of1(&array.buffer());
    if let Err(err) = scope.post_message_with_transfer(&array, &transfer) {
        tracing::warn!(?err, "postMessage to the UI failed");
    }
}

fn init_tracing() {
    let console = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .without_time()
        .with_writer(tracing_web::MakeWebConsoleWriter::new());
    tracing_subscriber::registry()
        .with(EnvFilter::new(LOG_FILTER))
        .with(console)
        .init();
}
