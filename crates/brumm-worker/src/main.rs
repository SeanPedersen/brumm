//! Browser build: the core's thread. The UI (brumm-app on the page's main thread) posts
//! [`ToWorker`] messages here and receives [`FromWorker`] messages back, both as postcard bytes.
//! This mirrors the desktop `bridge.rs`, where the core runs on its own tokio thread.
//!
//! The core runs in a worker because SQLite's OPFS storage (sahpool VFS) needs synchronous file
//! handles, which browsers only provide to dedicated workers. Each profile gets its own OPFS
//! directory, so two tabs with different profiles do not contend for the same pool lock.

#[cfg(target_family = "wasm")]
mod worker;

#[cfg(target_family = "wasm")]
fn main() {
    worker::main();
}

#[cfg(not(target_family = "wasm"))]
fn main() {
    eprintln!(
        "brumm-worker only runs in the browser; build it with `trunk` (see crates/brumm-app/index.html)"
    );
}
