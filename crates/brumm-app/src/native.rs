//! Desktop entry point.
//!
//! `brumm [--profile <name>]` — profiles keep separate identities/data dirs, which lets you
//! run two instances on one machine to try chatting with yourself.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::{app, bridge::CoreBridge, window_size};

const DEFAULT_PROFILE: &str = "default";
const PROFILE_FLAG: &str = "--profile";
const DEFAULT_LOG_FILTER: &str = "brumm=info,brumm_core=info,warn";
/// Matches `brumm.desktop` so Wayland/X11 shells pick up the icon for the taskbar.
const APP_ID: &str = "brumm";
const ICON_PNG: &[u8] = include_bytes!("../assets/icon.png");

pub fn main() -> Result<()> {
    init_tracing();
    let profile =
        profile_from_args(std::env::args().skip(1)).unwrap_or_else(|| DEFAULT_PROFILE.to_owned());
    let data_dir = data_dir(&profile)?;
    tracing::info!(profile, data_dir = %data_dir.display(), "starting brumm");

    // Eight bytes, before the window exists. SQLite is not open yet.
    let (window_size, size) = window_size::WindowSize::load(&data_dir);
    let mut viewport = egui::ViewportBuilder::default()
        .with_title(window_title(&profile))
        .with_app_id(APP_ID)
        .with_inner_size(size)
        .with_min_inner_size(window_size::MIN_SIZE);
    if let Some(icon) = window_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions {
        viewport,
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "brumm",
        options,
        Box::new(move |cc| {
            let core = CoreBridge::start(data_dir, cc.egui_ctx.clone());
            Ok(Box::new(app::BrummApp::new(cc, core, window_size)))
        }),
    )
    .map_err(|err| anyhow::anyhow!("ui failed: {err}"))
}

fn init_tracing() {
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(DEFAULT_LOG_FILTER));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}

fn profile_from_args(mut args: impl Iterator<Item = String>) -> Option<String> {
    while let Some(arg) = args.next() {
        if arg == PROFILE_FLAG {
            return args.next();
        }
        if let Some(value) = arg.strip_prefix("--profile=") {
            return Some(value.to_owned());
        }
    }
    None
}

fn data_dir(profile: &str) -> Result<PathBuf> {
    let dirs =
        directories::ProjectDirs::from("chat", "brumm", "brumm").context("no home directory")?;
    Ok(dirs.data_dir().join("profiles").join(profile))
}

fn window_icon() -> Option<egui::IconData> {
    let rgba = image::load_from_memory(ICON_PNG).ok()?.into_rgba8();
    let (width, height) = rgba.dimensions();
    Some(egui::IconData {
        rgba: rgba.into_raw(),
        width,
        height,
    })
}

fn window_title(profile: &str) -> String {
    if profile == DEFAULT_PROFILE {
        "brumm".to_owned()
    } else {
        format!("brumm — {profile}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(raw: &[&str]) -> impl Iterator<Item = String> {
        raw.iter()
            .map(|arg| arg.to_string())
            .collect::<Vec<_>>()
            .into_iter()
    }

    #[test]
    fn parses_profile_flag_forms() {
        assert_eq!(
            profile_from_args(args(&["--profile", "bob"])),
            Some("bob".into())
        );
        assert_eq!(
            profile_from_args(args(&["--profile=alice"])),
            Some("alice".into())
        );
        assert_eq!(profile_from_args(args(&[])), None);
    }
}
