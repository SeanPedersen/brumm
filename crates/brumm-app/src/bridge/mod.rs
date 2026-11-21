//! Non-blocking command/event channel between the UI and the core. Natively the core runs on
//! its own tokio thread; in the browser it runs in the brumm-worker Web Worker. Both expose the
//! same `CoreBridge` API (`send`, `drain`).

#[cfg(not(target_family = "wasm"))]
mod native;
#[cfg(not(target_family = "wasm"))]
mod profile_lock;
#[cfg(not(target_family = "wasm"))]
pub use native::CoreBridge;

#[cfg(target_family = "wasm")]
mod web;
#[cfg(target_family = "wasm")]
pub use web::CoreBridge;
