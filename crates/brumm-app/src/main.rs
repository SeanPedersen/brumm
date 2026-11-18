//! brumm desktop app entry point.
//!
//! `brumm [--profile <name>]` — profiles keep separate identities/data dirs, which lets you
//! run two instances on one machine to try chatting with yourself.

mod app;
mod bridge;
mod state;
mod ui;

use std::path::PathBuf;

use anyhow::{Context, Result};

const DEFAULT_PROFILE: &str = "default";
const PROFILE_FLAG: &str = "--profile";
const DEFAULT_LOG_FILTER: &str = "brumm=info,brumm_core=info,warn";
const WINDOW_SIZE: [f32; 2] = [960.0, 680.0];
const MIN_WINDOW_SIZE: [f32; 2] = [560.0, 400.0];

fn main() -> Result<()> {
    init_tracing();
    let profile = profile_from_args(std::env::args().skip(1)).unwrap_or_else(|| DEFAULT_PROFILE.to_owned());
    let data_dir = data_dir(&profile)?;
    tracing::info!(profile, data_dir = %data_dir.display(), "starting brumm");

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title(window_title(&profile))
            .with_inner_size(WINDOW_SIZE)
            .with_min_inner_size(MIN_WINDOW_SIZE),
        ..Default::default()
    };
    eframe::run_native(
        "brumm",
        options,
        Box::new(move |cc| Ok(Box::new(app::BrummApp::new(cc, data_dir)))),
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
    let dirs = directories::ProjectDirs::from("chat", "brumm", "brumm").context("no home directory")?;
    Ok(dirs.data_dir().join("profiles").join(profile))
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
        raw.iter().map(|arg| arg.to_string()).collect::<Vec<_>>().into_iter()
    }

    #[test]
    fn parses_profile_flag_forms() {
        assert_eq!(profile_from_args(args(&["--profile", "bob"])), Some("bob".into()));
        assert_eq!(profile_from_args(args(&["--profile=alice"])), Some("alice".into()));
        assert_eq!(profile_from_args(args(&[])), None);
    }
}
