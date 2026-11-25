//! brumm app entry point: the desktop window natively, an egui canvas in the browser (#2).
//! Each target's startup lives in its own module; everything below `app` is shared.

#![deny(unsafe_code)]

mod app;
mod bridge;
mod history;
mod page;
mod preview;
mod state;
mod touch;
mod ui;
mod version;
mod web_files;

#[cfg(not(target_family = "wasm"))]
mod native;
#[cfg(target_family = "wasm")]
mod web;
#[cfg(not(target_family = "wasm"))]
mod window_size;

#[cfg(not(target_family = "wasm"))]
fn main() -> anyhow::Result<()> {
    native::main()
}

#[cfg(target_family = "wasm")]
fn main() {
    web::main();
}
