//! Last window size, stored as 8 bytes next to the profile so startup does not open SQLite.
//!
//! [`WindowSize::load`] reads the file once, before the window exists. A resize is written only
//! after it settles, and again on exit. A missing or implausible file falls back to [`DEFAULT_SIZE`].

use std::{
    io::{Read as _, Write as _},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

pub const DEFAULT_SIZE: [f32; 2] = [960.0, 680.0];
pub const MIN_SIZE: [f32; 2] = [560.0, 400.0];

const FILE_NAME: &str = "window-size";
const FILE_BYTES: usize = 8;
/// Largest edge we restore. Anything bigger is treated as a corrupt file.
const MAX_EDGE: u32 = 16_384;
/// Wait until a drag stops before touching disk. Exit flushes immediately.
const WRITE_DELAY: Duration = Duration::from_millis(400);

/// Remembers the inner size (logical points) of this profile's window.
pub struct WindowSize {
    path: PathBuf,
    saved: [u32; 2],
    pending: Option<[u32; 2]>,
    pending_since: Instant,
}

impl WindowSize {
    /// Reads the saved size. The returned points are what the viewport should open at.
    pub fn load(data_dir: &Path) -> (Self, [f32; 2]) {
        let path = data_dir.join(FILE_NAME);
        let saved = read_size(&path).unwrap_or(default_edges());
        let size = [saved[0] as f32, saved[1] as f32];
        (
            Self {
                path,
                saved,
                pending: None,
                pending_since: Instant::now(),
            },
            size,
        )
    }

    /// Notes the current inner size. Does not touch disk.
    pub fn observe(&mut self, points: egui::Vec2) {
        let Some(edges) = edges_from_points(points) else { return };
        if edges == self.pending.unwrap_or(self.saved) {
            return;
        }
        if self.pending.is_none() {
            self.pending_since = Instant::now();
        }
        self.pending = Some(edges);
    }

    /// Writes once the size has stopped changing.
    pub fn flush_if_due(&mut self) {
        if self.pending.is_none() || self.pending_since.elapsed() < WRITE_DELAY {
            return;
        }
        self.flush();
    }

    /// Writes a pending size now.
    pub fn flush(&mut self) {
        let Some(edges) = self.pending.take() else { return };
        if edges == self.saved {
            return;
        }
        if let Err(err) = write_size(&self.path, edges) {
            self.pending = Some(edges);
            self.pending_since = Instant::now();
            tracing::warn!(path = %self.path.display(), %err, "could not save window size");
            return;
        }
        self.saved = edges;
    }
}

impl Drop for WindowSize {
    fn drop(&mut self) {
        self.flush();
    }
}

fn default_edges() -> [u32; 2] {
    [DEFAULT_SIZE[0] as u32, DEFAULT_SIZE[1] as u32]
}

fn edges_from_points(points: egui::Vec2) -> Option<[u32; 2]> {
    if !points.x.is_finite() || !points.y.is_finite() {
        return None;
    }
    if points.x < MIN_SIZE[0] || points.y < MIN_SIZE[1] || points.x > MAX_EDGE as f32 || points.y > MAX_EDGE as f32
    {
        return None;
    }
    Some([points.x.round() as u32, points.y.round() as u32])
}

fn read_size(path: &Path) -> Option<[u32; 2]> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut bytes = [0u8; FILE_BYTES];
    file.read_exact(&mut bytes).ok()?;
    decode(bytes)
}

fn decode(bytes: [u8; FILE_BYTES]) -> Option<[u32; 2]> {
    let width = u32::from_le_bytes(bytes[..4].try_into().ok()?);
    let height = u32::from_le_bytes(bytes[4..].try_into().ok()?);
    let min_width = MIN_SIZE[0] as u32;
    let min_height = MIN_SIZE[1] as u32;
    if width < min_width || height < min_height || width > MAX_EDGE || height > MAX_EDGE {
        return None;
    }
    Some([width, height])
}

fn write_size(path: &Path, edges: [u32; 2]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut bytes = [0u8; FILE_BYTES];
    bytes[..4].copy_from_slice(&edges[0].to_le_bytes());
    bytes[4..].copy_from_slice(&edges[1].to_le_bytes());
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(&bytes)?;
    drop(file);
    std::fs::rename(&tmp, path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pack(width: u32, height: u32) -> [u8; FILE_BYTES] {
        let mut bytes = [0u8; FILE_BYTES];
        bytes[..4].copy_from_slice(&width.to_le_bytes());
        bytes[4..].copy_from_slice(&height.to_le_bytes());
        bytes
    }

    #[test]
    fn rejects_small_and_huge_sizes() {
        assert_eq!(decode(pack(960, 680)), Some([960, 680]));
        assert_eq!(decode(pack(1, 1)), None);
        assert_eq!(decode(pack(20_000, 680)), None);
    }

    #[test]
    fn load_falls_back_and_round_trips_a_resize() {
        let dir = std::env::temp_dir().join(format!("brumm-window-size-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let (mut store, size) = WindowSize::load(&dir);
        assert_eq!(size, DEFAULT_SIZE);

        store.observe(egui::vec2(100.0, 100.0));
        store.flush();
        assert!(!dir.join(FILE_NAME).exists(), "below the minimum size is not saved");

        store.observe(egui::vec2(1100.4, 700.6));
        store.flush();
        let (_, restored) = WindowSize::load(&dir);
        assert_eq!(restored, [1100.0, 701.0]);

        std::fs::write(dir.join(FILE_NAME), [9u8, 9, 9]).unwrap();
        assert_eq!(WindowSize::load(&dir).1, DEFAULT_SIZE);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
