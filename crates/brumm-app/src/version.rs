//! The version shown at the top right of the ☰ menu: crate version plus the build id from
//! `build.rs`, so every device (desktop, browser) can be checked to run the same build.

/// Compared with the deployed `version.txt` by the browser tab (`page.rs`).
#[cfg(target_family = "wasm")]
pub const BUILD_ID: &str = env!("BRUMM_BUILD_ID");
pub const LABEL: &str = concat!(
    "v",
    env!("CARGO_PKG_VERSION"),
    " · ",
    env!("BRUMM_BUILD_ID")
);

#[cfg(test)]
mod tests {
    #[test]
    fn label_names_the_crate_version_and_a_build() {
        let label = super::LABEL;
        assert!(
            label.starts_with(concat!("v", env!("CARGO_PKG_VERSION"), " · ")),
            "{label}"
        );
        assert!(!env!("BRUMM_BUILD_ID").is_empty());
    }
}
