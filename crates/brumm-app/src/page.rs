//! Browser page lifecycle. When the tab comes back to the foreground, is restored from the
//! back-forward cache, gets the network back, and every few minutes while visible:
//!
//! - the app sends [`brumm_core::api::Command::Resume`]: a browser cannot ping its WebSockets,
//!   so a suspended phone tab may keep dead relay connections that never report an error, and
//!   letters would wait until the sender reaches this device directly;
//! - a tab whose build differs from the deployed `version.txt` reloads itself, since an old
//!   worker cannot read what newer clients send.
//!
//! On the desktop there is nothing to do: native relay connections are pinged.

#[cfg(target_family = "wasm")]
mod imp {
    use std::{
        cell::Cell,
        sync::atomic::{AtomicBool, Ordering},
    };

    use wasm_bindgen::{JsCast, closure::Closure};
    use wasm_bindgen_futures::JsFuture;

    use crate::version::BUILD_ID;

    const VERSION_PATH: &str = "version.txt";
    const PERIODIC_CHECK_MS: i32 = 5 * 60 * 1000;
    /// Visibility, `pageshow` and `online` often fire together; one resume covers them.
    const MIN_RESUME_GAP_MS: f64 = 5_000.0;
    const VISIBILITY_EVENT: &str = "visibilitychange";
    const PAGE_SHOW_EVENT: &str = "pageshow";
    const ONLINE_EVENT: &str = "online";

    static RESUMED: AtomicBool = AtomicBool::new(false);

    thread_local! {
        static LAST_RESUME_MS: Cell<f64> = const { Cell::new(f64::NEG_INFINITY) };
    }

    pub fn install(ctx: egui::Context) {
        let Some(window) = web_sys::window() else {
            return;
        };
        let listen = |target: &web_sys::EventTarget, event: &str, ctx: egui::Context| {
            let callback = Closure::<dyn FnMut()>::new(move || {
                if is_visible() {
                    wake(&ctx);
                }
            });
            let _ =
                target.add_event_listener_with_callback(event, callback.as_ref().unchecked_ref());
            callback.forget();
        };
        if let Some(document) = window.document() {
            listen(&document, VISIBILITY_EVENT, ctx.clone());
        }
        listen(&window, PAGE_SHOW_EVENT, ctx.clone());
        listen(&window, ONLINE_EVENT, ctx.clone());
        let on_timer = Closure::<dyn FnMut()>::new(move || {
            if is_visible() {
                wake(&ctx);
            }
        });
        let _ = window.set_interval_with_callback_and_timeout_and_arguments_0(
            on_timer.as_ref().unchecked_ref(),
            PERIODIC_CHECK_MS,
        );
        on_timer.forget();
    }

    /// True once after the page came back; the app then sends `Command::Resume`.
    pub fn take_resumed() -> bool {
        RESUMED.swap(false, Ordering::Relaxed)
    }

    fn is_visible() -> bool {
        web_sys::window()
            .and_then(|window| window.document())
            .is_some_and(|document| {
                document.visibility_state() == web_sys::VisibilityState::Visible
            })
    }

    fn wake(ctx: &egui::Context) {
        let now = js_sys::Date::now();
        let recent = LAST_RESUME_MS.with(|last| {
            let recent = now - last.get() < MIN_RESUME_GAP_MS;
            if !recent {
                last.set(now);
            }
            recent
        });
        if recent {
            return;
        }
        RESUMED.store(true, Ordering::Relaxed);
        ctx.request_repaint();
        wasm_bindgen_futures::spawn_local(reload_if_outdated());
    }

    async fn reload_if_outdated() {
        let Some(deployed) = deployed_build().await else {
            return;
        };
        if deployed.is_empty() || deployed == BUILD_ID {
            return;
        }
        tracing::info!(
            running = BUILD_ID,
            deployed,
            "a newer build is deployed, reloading"
        );
        if let Some(window) = web_sys::window() {
            let _ = window.location().reload();
        }
    }

    /// The deployed build id; `None` when offline or not deployed (`trunk serve`).
    async fn deployed_build() -> Option<String> {
        let window = web_sys::window()?;
        // A unique query keeps every cache (browser, CDN) out of the way.
        let url = format!("{VERSION_PATH}?now={}", js_sys::Date::now() as u64);
        let response: web_sys::Response = JsFuture::from(window.fetch_with_str(&url))
            .await
            .ok()?
            .dyn_into()
            .ok()?;
        if !response.ok() {
            return None;
        }
        let text = JsFuture::from(response.text().ok()?).await.ok()?;
        Some(text.as_string()?.trim().to_owned())
    }
}

#[cfg(not(target_family = "wasm"))]
mod imp {
    pub fn take_resumed() -> bool {
        false
    }
}

#[cfg(target_family = "wasm")]
pub use imp::install;
pub use imp::take_resumed;
