//! Browser entry point: mounts the egui app on the page's canvas.
//!
//! `?profile=<name>` in the URL picks a separate identity and database (like `--profile` on
//! the desktop), so two tabs can chat with each other.

use tracing_subscriber::{EnvFilter, layer::SubscriberExt, util::SubscriberInitExt};
use wasm_bindgen::JsCast;
use web_sys::HtmlCanvasElement;

use crate::{app, bridge::CoreBridge};

const CANVAS_ID: &str = "brumm-canvas";
const DEFAULT_PROFILE: &str = "default";
const PROFILE_PARAM: &str = "profile";
const LOG_FILTER: &str = "brumm=info,warn";

brumm_core::register_browser_clock!();

pub fn main() {
    console_error_panic_hook::set_once();
    init_tracing();
    let profile = profile_from_url().unwrap_or_else(|| DEFAULT_PROFILE.to_owned());
    tracing::info!(profile, "starting brumm web");
    let window = web_sys::window().expect("running in a browser window");
    if profile != DEFAULT_PROFILE
        && let Some(document) = window.document()
    {
        document.set_title(&format!("brumm — {profile}"));
    }
    let canvas = window
        .document()
        .and_then(|document| document.get_element_by_id(CANVAS_ID))
        .and_then(|element| element.dyn_into::<HtmlCanvasElement>().ok())
        .expect("index.html has the brumm canvas");

    wasm_bindgen_futures::spawn_local(async move {
        let started = eframe::WebRunner::new()
            .start(
                canvas,
                eframe::WebOptions::default(),
                Box::new(move |cc| {
                    crate::history::install(cc.egui_ctx.clone());
                    let core = CoreBridge::start(profile, cc.egui_ctx.clone());
                    Ok(Box::new(app::BrummApp::new(cc, core)))
                }),
            )
            .await;
        if let Err(err) = started {
            tracing::error!(?err, "egui failed to start");
        }
    });
}

fn profile_from_url() -> Option<String> {
    let search = web_sys::window()?.location().search().ok()?;
    let profile = web_sys::UrlSearchParams::new_with_str(&search)
        .ok()?
        .get(PROFILE_PARAM)?;
    let valid = !profile.is_empty()
        && profile
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    valid.then_some(profile)
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
