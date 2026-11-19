//! Native file dialogs (run on helper threads so the UI never blocks) and the file/image
//! preparation that goes with them: reading an attachment and shrinking a profile photo.

use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver, Sender},
};

use brumm_core::{
    identity::UserKey,
    wire::{MAX_AVATAR_BYTES, MAX_FILE_BYTES, MessageId},
};
use image::{DynamicImage, ImageEncoder, codecs::jpeg::JpegEncoder, imageops::FilterType};

/// Candidate avatar edge lengths, largest first; the first one that fits the size budget wins.
const AVATAR_EDGE_STEPS: [u32; 5] = [192, 160, 128, 96, 64];
const AVATAR_JPEG_QUALITY: u8 = 85;
const IMAGE_EXTENSIONS: [&str; 4] = ["png", "jpg", "jpeg", "webp"];

/// Outcome of a dialog the user confirmed.
pub enum Picked {
    Attachment { contact: UserKey, path: PathBuf },
    Avatar(PathBuf),
    SaveAs { contact: UserKey, id: MessageId, path: PathBuf },
}

pub struct FileDialogs {
    tx: Sender<Picked>,
    rx: Receiver<Picked>,
}

impl Default for FileDialogs {
    fn default() -> Self {
        let (tx, rx) = mpsc::channel();
        Self { tx, rx }
    }
}

impl FileDialogs {
    pub fn pick_attachment(&self, ctx: &egui::Context, contact: UserKey) {
        self.spawn(ctx, move || {
            rfd::FileDialog::new().set_title("Send file").pick_file().map(|path| Picked::Attachment { contact, path })
        });
    }

    pub fn pick_avatar(&self, ctx: &egui::Context) {
        self.spawn(ctx, || {
            rfd::FileDialog::new()
                .set_title("Choose a profile photo")
                .add_filter("Images", &IMAGE_EXTENSIONS)
                .pick_file()
                .map(Picked::Avatar)
        });
    }

    pub fn pick_save_path(&self, ctx: &egui::Context, contact: UserKey, id: MessageId, file_name: String) {
        self.spawn(ctx, move || {
            rfd::FileDialog::new()
                .set_title("Save file")
                .set_file_name(file_name)
                .save_file()
                .map(|path| Picked::SaveAs { contact, id, path })
        });
    }

    pub fn poll(&self) -> Vec<Picked> {
        self.rx.try_iter().collect()
    }

    fn spawn(&self, ctx: &egui::Context, dialog: impl FnOnce() -> Option<Picked> + Send + 'static) {
        let (tx, ctx) = (self.tx.clone(), ctx.clone());
        std::thread::spawn(move || {
            if let Some(picked) = dialog() {
                let _ = tx.send(picked);
                ctx.request_repaint();
            }
        });
    }
}

pub fn is_image_name(name: &str) -> bool {
    Path::new(name)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| IMAGE_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str()))
}

/// Reads a file to send: its display name and bytes, rejecting anything over the size limit
/// before it is loaded.
pub fn read_attachment(path: &Path) -> Result<(String, Vec<u8>), String> {
    let metadata = std::fs::metadata(path).map_err(|err| format!("cannot read file: {err}"))?;
    if !metadata.is_file() {
        return Err("only files can be sent".to_owned());
    }
    if metadata.len() > MAX_FILE_BYTES as u64 {
        return Err(format!("file is too large (limit {})", format_size(MAX_FILE_BYTES)));
    }
    let name = path.file_name().and_then(|name| name.to_str()).unwrap_or("file").to_owned();
    let data = std::fs::read(path).map_err(|err| format!("cannot read file: {err}"))?;
    Ok((name, data))
}

/// Center-crops to a square and shrinks until the JPEG fits the avatar budget.
pub fn avatar_from_file(path: &Path) -> Result<Vec<u8>, String> {
    let source = image::open(path).map_err(|err| format!("cannot open image: {err}"))?;
    let square = center_square(source);
    for edge in AVATAR_EDGE_STEPS {
        let resized = square.resize_exact(edge, edge, FilterType::Lanczos3).to_rgb8();
        let mut encoded = Vec::new();
        let encoder = JpegEncoder::new_with_quality(&mut encoded, AVATAR_JPEG_QUALITY);
        encoder
            .write_image(resized.as_raw(), edge, edge, image::ExtendedColorType::Rgb8)
            .map_err(|err| format!("cannot encode image: {err}"))?;
        if encoded.len() <= MAX_AVATAR_BYTES {
            return Ok(encoded);
        }
    }
    Err("image could not be shrunk enough for a profile photo".to_owned())
}

fn center_square(image: DynamicImage) -> DynamicImage {
    let edge = image.width().min(image.height());
    let (x, y) = ((image.width() - edge) / 2, (image.height() - edge) / 2);
    image.crop_imm(x, y, edge, edge)
}

/// "1.4 MB", "820 KB", "12 B"
pub fn format_size(bytes: usize) -> String {
    const KIB: f64 = 1024.0;
    let bytes_f = bytes as f64;
    if bytes_f >= KIB * KIB {
        format!("{:.1} MB", bytes_f / (KIB * KIB))
    } else if bytes_f >= KIB {
        format!("{:.0} KB", bytes_f / KIB)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_images_by_extension() {
        assert!(is_image_name("Holiday.JPG"));
        assert!(!is_image_name("notes.txt"));
        assert!(!is_image_name("png"));
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(12), "12 B");
        assert_eq!(format_size(2048), "2 KB");
        assert_eq!(format_size(3 * 1024 * 1024), "3.0 MB");
    }
}
