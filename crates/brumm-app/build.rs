//! Build id for the app menu and the web client's reload check (`src/page.rs`):
//! `<commit count>-<short hash>`, so a larger count is a newer build. `deploy/deploy.sh` passes
//! the same id in `BRUMM_BUILD_ID` and writes it to `version.txt`.

use std::{path::Path, process::Command};

const UNKNOWN_BUILD: &str = "dev";

fn main() {
    println!("cargo:rerun-if-env-changed=BRUMM_BUILD_ID");
    let git_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../.git");
    for tracked in ["HEAD", "refs/heads", "packed-refs"] {
        let path = git_dir.join(tracked);
        if path.exists() {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
    let id = std::env::var("BRUMM_BUILD_ID")
        .ok()
        .filter(|id| !id.is_empty())
        .or_else(git_build_id)
        .unwrap_or_else(|| UNKNOWN_BUILD.to_owned());
    println!("cargo:rustc-env=BRUMM_BUILD_ID={id}");
}

fn git_build_id() -> Option<String> {
    let count = git(&["rev-list", "--count", "HEAD"])?;
    let hash = git(&["rev-parse", "--short", "HEAD"])?;
    Some(format!("{count}-{hash}"))
}

fn git(args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .args(args)
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_owned();
    (!text.is_empty()).then_some(text)
}
