//! Native file dialogs (run on helper threads so the UI never blocks) and the file/image
//! preparation that goes with them: reading an attachment, shrinking a profile photo, and
//! building a bubble-sized preview so a large photo is not decoded at full resolution.

use std::{
    io::Cursor,
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
    Attachment {
        contact: UserKey,
        path: PathBuf,
    },
    Avatar(PathBuf),
    SaveAs {
        contact: UserKey,
        id: MessageId,
        path: PathBuf,
    },
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
            rfd::FileDialog::new()
                .set_title("Send file")
                .pick_file()
                .map(|path| Picked::Attachment { contact, path })
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

    pub fn pick_save_path(
        &self,
        ctx: &egui::Context,
        contact: UserKey,
        id: MessageId,
        file_name: String,
    ) {
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
        .is_some_and(|extension| {
            IMAGE_EXTENSIONS.contains(&extension.to_ascii_lowercase().as_str())
        })
}

/// Reads a file to send: its display name and bytes, rejecting anything over the size limit
/// before it is loaded.
pub fn read_attachment(path: &Path) -> Result<(String, Vec<u8>), String> {
    let metadata = std::fs::metadata(path).map_err(|err| format!("cannot read file: {err}"))?;
    if !metadata.is_file() {
        return Err("only files can be sent".to_owned());
    }
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("file")
        .to_owned();
    if metadata.len() > MAX_FILE_BYTES as u64 {
        if !is_image_name(&name) {
            return Err(format!(
                "file is too large (limit {})",
                format_size(MAX_FILE_BYTES)
            ));
        }
        let source = image::open(path).map_err(|err| format!("cannot open image: {err}"))?;
        return shrink_to_fit(
            &source,
            Path::new(&name)
                .file_stem()
                .and_then(|stem| stem.to_str())
                .unwrap_or("image"),
        );
    }
    let data = std::fs::read(path).map_err(|err| format!("cannot read file: {err}"))?;
    Ok((name, data))
}

/// What a Ctrl+V found on the clipboard that egui itself cannot see.
pub enum ClipboardItem {
    File(PathBuf),
    /// Encoded image bytes (PNG).
    Image(Vec<u8>),
}

const URI_LIST_MIME: &str = "text/uri-list";
const PNG_MIME: &str = "image/png";
const FILE_URI_PREFIX: &str = "file://";

/// Files copied in a file manager or an image copied as PNG. On Wayland the compositor may offer no
/// data-control protocol (KDE), which arboard needs, so `wl-paste` is asked first there.
pub fn clipboard_items() -> Vec<ClipboardItem> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return wl_paste_items();
    }
    let files = arboard::Clipboard::new()
        .and_then(|mut clipboard| clipboard.get().file_list())
        .unwrap_or_default();
    files.into_iter().map(ClipboardItem::File).collect()
}

fn wl_paste_items() -> Vec<ClipboardItem> {
    if let Some(list) = wl_paste(URI_LIST_MIME) {
        let files: Vec<_> = String::from_utf8_lossy(&list)
            .lines()
            .filter_map(path_from_uri)
            .map(ClipboardItem::File)
            .collect();
        if !files.is_empty() {
            return files;
        }
    }
    wl_paste(PNG_MIME)
        .map(ClipboardItem::Image)
        .into_iter()
        .collect()
}

fn wl_paste(mime: &str) -> Option<Vec<u8>> {
    let output = std::process::Command::new("wl-paste")
        .args(["--no-newline", "--type", mime])
        .output()
        .inspect_err(|err| tracing::warn!(%err, "wl-paste failed to start"))
        .ok()?;
    tracing::debug!(mime, status = %output.status, bytes = output.stdout.len(), stderr = %String::from_utf8_lossy(&output.stderr), "wl-paste");
    (output.status.success() && !output.stdout.is_empty()).then_some(output.stdout)
}

/// `file:///a/b%20c.png` -> `/a/b c.png`; comments and other schemes yield `None`.
fn path_from_uri(line: &str) -> Option<PathBuf> {
    let encoded = line.trim().strip_prefix(FILE_URI_PREFIX)?;
    let mut bytes = Vec::with_capacity(encoded.len());
    let mut input = encoded.bytes();
    while let Some(byte) = input.next() {
        if byte != b'%' {
            bytes.push(byte);
            continue;
        }
        let hex: Vec<u8> = input.by_ref().take(2).collect();
        let decoded = std::str::from_utf8(&hex)
            .ok()
            .and_then(|digits| u8::from_str_radix(digits, 16).ok())?;
        bytes.push(decoded);
    }
    Some(PathBuf::from(String::from_utf8(bytes).ok()?))
}

const PASTED_IMAGE_STEM: &str = "pasted-image";
const PASTED_IMAGE_NAME: &str = "pasted-image.png";
const SHRINK_SCALES: [f32; 7] = [1.0, 0.75, 0.55, 0.4, 0.3, 0.2, 0.12];
const SHRUNK_JPEG_QUALITY: u8 = 85;
/// Longest edge of a chat image bubble, in logical pixels.
pub const IMAGE_BUBBLE_EDGE: f32 = 320.0;
/// Physical-pixel cap for a preview (a 3× display of the bubble). Above this the texture
/// costs RAM the bubble cannot show.
const PREVIEW_PIXEL_CAP: u32 = 960;
const PREVIEW_JPEG_QUALITY: u8 = 80;

/// Source pixel size, plus a display image when the original is larger than the bubble.
pub struct ChatImage {
    pub size: (u32, u32),
    /// Encoded preview. `None` when the original is already small enough to decode as-is.
    pub preview: Option<Vec<u8>>,
}

/// Longest edge of the texture, in physical pixels, for the current screen scale.
pub fn preview_max_edge(pixels_per_point: f32) -> u32 {
    let scaled = (IMAGE_BUBBLE_EDGE * pixels_per_point.max(1.0)).ceil() as u32;
    scaled.clamp(IMAGE_BUBBLE_EDGE as u32, PREVIEW_PIXEL_CAP)
}

/// Reads an image header and, when the photo is larger than `max_edge`, re-encodes a preview.
/// The full-resolution bitmap is dropped before this returns. `None` means the bytes are not an image.
pub fn prepare_chat_image(bytes: &[u8], max_edge: u32) -> Option<ChatImage> {
    let max_edge = max_edge.max(1);
    let (width, height) = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .ok()?
        .into_dimensions()
        .ok()?;
    if width == 0 || height == 0 {
        return None;
    }
    if width.max(height) <= max_edge {
        return Some(ChatImage {
            size: (width, height),
            preview: None,
        });
    }
    let scale = max_edge as f32 / width.max(height) as f32;
    let target_w = ((width as f32 * scale).round() as u32).max(1);
    let target_h = ((height as f32 * scale).round() as u32).max(1);
    // Scope the decode so the full bitmap is freed before the preview is encoded.
    let resized = {
        let decoded = image::load_from_memory(bytes).ok()?;
        decoded.thumbnail(target_w, target_h)
    };
    let preview = encode_preview(resized)?;
    Some(ChatImage {
        size: (width, height),
        preview: Some(preview),
    })
}

/// JPEG for opaque photos, PNG when the source has alpha (a JPEG would turn it black).
fn encode_preview(image: DynamicImage) -> Option<Vec<u8>> {
    let mut encoded = Vec::new();
    let (width, height) = (image.width(), image.height());
    if image.color().has_alpha() {
        let rgba = image.into_rgba8();
        image::codecs::png::PngEncoder::new(&mut encoded)
            .write_image(
                rgba.as_raw(),
                width,
                height,
                image::ExtendedColorType::Rgba8,
            )
            .ok()?;
    } else {
        let rgb = image.into_rgb8();
        JpegEncoder::new_with_quality(&mut encoded, PREVIEW_JPEG_QUALITY)
            .write_image(rgb.as_raw(), width, height, image::ExtendedColorType::Rgb8)
            .ok()?;
    }
    Some(encoded)
}

/// Clipboard pixels as an attachment: PNG when it fits the file limit, otherwise a shrunken JPEG.
pub fn attachment_from_pixels(image: &egui::ColorImage) -> Result<(String, Vec<u8>), String> {
    let [width, height] = image.size;
    let rgba = image::RgbaImage::from_raw(width as u32, height as u32, image.as_raw().to_vec())
        .ok_or_else(|| "cannot read pasted image".to_owned())?;
    attachment_from_decoded(DynamicImage::ImageRgba8(rgba))
}

/// Clipboard image file bytes (e.g. PNG) as an attachment, shrunk when over the file limit.
pub fn attachment_from_encoded(bytes: Vec<u8>) -> Result<(String, Vec<u8>), String> {
    if bytes.len() <= MAX_FILE_BYTES {
        return Ok((PASTED_IMAGE_NAME.to_owned(), bytes));
    }
    let decoded = image::load_from_memory(&bytes)
        .map_err(|err| format!("cannot read pasted image: {err}"))?;
    shrink_to_fit(&decoded, PASTED_IMAGE_STEM)
}

fn attachment_from_decoded(decoded: DynamicImage) -> Result<(String, Vec<u8>), String> {
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(
            decoded.as_bytes(),
            decoded.width(),
            decoded.height(),
            decoded.color().into(),
        )
        .map_err(|err| format!("cannot encode pasted image: {err}"))?;
    if png.len() <= MAX_FILE_BYTES {
        return Ok((PASTED_IMAGE_NAME.to_owned(), png));
    }
    shrink_to_fit(&decoded, PASTED_IMAGE_STEM)
}

/// Re-encodes as JPEG at progressively smaller sizes until it fits the file limit.
fn shrink_to_fit(source: &DynamicImage, stem: &str) -> Result<(String, Vec<u8>), String> {
    let rgb = DynamicImage::ImageRgb8(source.to_rgb8());
    for scale in SHRINK_SCALES {
        let width = ((rgb.width() as f32 * scale) as u32).max(1);
        let height = ((rgb.height() as f32 * scale) as u32).max(1);
        let resized = if scale < 1.0 {
            rgb.resize_exact(width, height, FilterType::Lanczos3)
        } else {
            rgb.clone()
        };
        let mut encoded = Vec::new();
        JpegEncoder::new_with_quality(&mut encoded, SHRUNK_JPEG_QUALITY)
            .write_image(
                resized.as_bytes(),
                width,
                height,
                image::ExtendedColorType::Rgb8,
            )
            .map_err(|err| format!("cannot encode image: {err}"))?;
        if encoded.len() <= MAX_FILE_BYTES {
            return Ok((format!("{stem}.jpg"), encoded));
        }
    }
    Err(format!(
        "image could not be shrunk below {}",
        format_size(MAX_FILE_BYTES)
    ))
}

/// Center-crops to a square and shrinks until the JPEG fits the avatar budget.
pub fn avatar_from_file(path: &Path) -> Result<Vec<u8>, String> {
    let source = image::open(path).map_err(|err| format!("cannot open image: {err}"))?;
    let square = center_square(source);
    for edge in AVATAR_EDGE_STEPS {
        let resized = square
            .resize_exact(edge, edge, FilterType::Lanczos3)
            .to_rgb8();
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
    fn oversized_images_are_shrunk_under_the_limit() {
        let noise = image::RgbImage::from_fn(1600, 1600, |x, y| {
            image::Rgb([((x * 7) ^ (y * 13)) as u8, (x * y) as u8, (x ^ y) as u8])
        });
        let (name, data) = shrink_to_fit(&DynamicImage::ImageRgb8(noise), "big").expect("shrinks");
        assert_eq!(name, "big.jpg");
        assert!(data.len() <= MAX_FILE_BYTES);
    }

    #[test]
    fn decodes_file_uris() {
        assert_eq!(
            path_from_uri("file:///home/a/b%20c.png\r"),
            Some(PathBuf::from("/home/a/b c.png"))
        );
        assert_eq!(path_from_uri("# comment"), None);
    }

    #[test]
    fn formats_sizes() {
        assert_eq!(format_size(12), "12 B");
        assert_eq!(format_size(2048), "2 KB");
        assert_eq!(format_size(3 * 1024 * 1024), "3.0 MB");
    }

    fn encode_rgb(width: u32, height: u32) -> Vec<u8> {
        let pixels = image::RgbImage::from_pixel(width, height, image::Rgb([40, 90, 140]));
        let mut bytes = Vec::new();
        pixels
            .write_to(&mut Cursor::new(&mut bytes), image::ImageFormat::Png)
            .expect("encode png");
        bytes
    }

    #[test]
    fn large_photos_become_a_bubble_sized_preview() {
        let bytes = encode_rgb(2000, 1000);
        let prepared = prepare_chat_image(&bytes, 320).expect("image");
        assert_eq!(prepared.size, (2000, 1000));
        let preview = prepared.preview.expect("downscaled");
        let (width, height) = image::ImageReader::new(Cursor::new(preview.as_slice()))
            .with_guessed_format()
            .unwrap()
            .into_dimensions()
            .unwrap();
        assert_eq!((width, height), (320, 160));
        assert!(preview.len() < 64 * 1024);
    }

    #[test]
    fn small_photos_keep_their_original_bytes() {
        let bytes = encode_rgb(64, 48);
        let prepared = prepare_chat_image(&bytes, 320).expect("image");
        assert_eq!(prepared.size, (64, 48));
        assert!(prepared.preview.is_none());
    }

    #[test]
    fn non_images_are_not_previews() {
        assert!(prepare_chat_image(b"not an image", 320).is_none());
    }
}
