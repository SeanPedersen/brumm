//! Browser history integration for the single-pane (phone) layout: opening a chat pushes a
//! history entry, so the system back gesture returns to the chat list instead of leaving the
//! page. On the desktop there is no history; the in-app back button acts directly.

#[cfg(target_family = "wasm")]
mod imp {
    use std::sync::atomic::{AtomicBool, Ordering};

    use wasm_bindgen::{JsCast, closure::Closure};

    static POPPED: AtomicBool = AtomicBool::new(false);
    const POPSTATE_EVENT: &str = "popstate";

    /// Listens for back navigation and wakes the UI, which only repaints on demand.
    pub fn install(ctx: egui::Context) {
        let Some(window) = web_sys::window() else {
            return;
        };
        let on_pop = Closure::<dyn FnMut()>::new(move || {
            POPPED.store(true, Ordering::Relaxed);
            ctx.request_repaint();
        });
        let _ = window
            .add_event_listener_with_callback(POPSTATE_EVENT, on_pop.as_ref().unchecked_ref());
        on_pop.forget();
    }

    pub fn push_chat() {
        if let Some(history) = web_sys::window().and_then(|window| window.history().ok()) {
            let _ = history.push_state(&wasm_bindgen::JsValue::NULL, "");
        }
    }

    /// Asks the browser to go back; the UI follows once `popstate` arrives.
    pub fn back() -> bool {
        let Some(history) = web_sys::window().and_then(|window| window.history().ok()) else {
            return false;
        };
        history.back().is_ok()
    }

    pub fn take_popped() -> bool {
        POPPED.swap(false, Ordering::Relaxed)
    }
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    pub fn push_chat() {}

    pub fn back() -> bool {
        false
    }

    pub fn take_popped() -> bool {
        false
    }
}

#[cfg(target_family = "wasm")]
pub use imp::install;
pub use imp::{back, push_chat, take_popped};
