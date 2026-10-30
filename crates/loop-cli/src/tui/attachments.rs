//! Images attached in the composer.
//!
//! An attached image shows in the input as a chip such as `[image #1]`. When
//! the message is sent, each chip still in the text sends its image as an
//! image content block; deleting the chip drops the image. An image file
//! named with `@path` is sent too.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use loop_protocol::ContentBlock;

/// The largest image file attached, in bytes.
pub const MAX_IMAGE_BYTES: u64 = 5 * 1024 * 1024;

/// The images attached to the message being written.
#[derive(Debug, Default)]
pub struct Attachments {
    next: usize,
    images: Vec<(String, ContentBlock)>,
}

impl Attachments {
    /// Attach `image` and return the chip to insert in the input.
    pub fn add(&mut self, image: ContentBlock) -> String {
        self.next += 1;
        let chip = format!("[image #{}]", self.next);
        self.images.push((chip.clone(), image));
        chip
    }

    /// The images whose chip is still in `text`, in order; forgets them all.
    pub fn take_used(&mut self, text: &str) -> Vec<ContentBlock> {
        std::mem::take(&mut self.images)
            .into_iter()
            .filter(|(chip, _)| text.contains(chip.as_str()))
            .map(|(_, image)| image)
            .collect()
    }

    /// Forget every attachment (the input was cleared).
    pub fn clear(&mut self) {
        self.images.clear();
    }
}

/// The image type for `path`'s extension, when it is one models read.
pub fn image_mime(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    Some(match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => return None,
    })
}

/// The image file at `path` (relative to `cwd`) as a content block.
pub fn image_file(cwd: &Path, path: &Path) -> Result<ContentBlock, String> {
    let path = cwd.join(path);
    let mime = image_mime(&path)
        .ok_or_else(|| format!("{} is not a PNG, JPEG, GIF or WebP image", path.display()))?;
    let size = std::fs::metadata(&path)
        .map_err(|e| format!("{}: {e}", path.display()))?
        .len();
    if size > MAX_IMAGE_BYTES {
        return Err(format!(
            "{} is {} MB; images can be at most {} MB",
            path.display(),
            size / (1024 * 1024),
            MAX_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    let bytes = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(ContentBlock::Image {
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
        mime_type: mime.into(),
    })
}

/// The image file a paste names, when the pasted text is just a path to
/// one (terminals paste a dragged file's path, sometimes quoted or with
/// `\ ` escapes).
pub fn pasted_image_path(cwd: &Path, text: &str) -> Option<PathBuf> {
    let text = text.trim();
    let unquoted = text
        .strip_prefix('\'')
        .and_then(|t| t.strip_suffix('\''))
        .or_else(|| text.strip_prefix('"').and_then(|t| t.strip_suffix('"')))
        .unwrap_or(text);
    let path = PathBuf::from(unquoted.replace("\\ ", " "));
    let path = path
        .strip_prefix("file://")
        .map(Path::to_path_buf)
        .unwrap_or(path);
    (image_mime(&path).is_some() && cwd.join(&path).is_file()).then_some(path)
}

/// The image files named with `@path` in `text` that exist.
pub fn mentioned_images(cwd: &Path, text: &str) -> Vec<PathBuf> {
    text.split_whitespace()
        .filter_map(|word| word.strip_prefix('@'))
        .map(PathBuf::from)
        .filter(|path| image_mime(path).is_some() && cwd.join(path).is_file())
        .collect()
}

/// RGBA pixels as a PNG content block.
pub fn png_block(width: u32, height: u32, rgba: &[u8]) -> Result<ContentBlock, String> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().map_err(|e| e.to_string())?;
        writer.write_image_data(rgba).map_err(|e| e.to_string())?;
    }
    Ok(ContentBlock::Image {
        data: base64::engine::general_purpose::STANDARD.encode(out),
        mime_type: "image/png".into(),
    })
}

/// The image on the system clipboard, as a PNG; `Ok(None)` when it holds
/// no image.
pub fn clipboard_image() -> Result<Option<ContentBlock>, String> {
    let mut clipboard = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    match clipboard.get_image() {
        Ok(image) => {
            let block = png_block(image.width as u32, image.height as u32, &image.bytes)?;
            if let ContentBlock::Image { data, .. } = &block {
                // Base64 is 4/3 of the PNG's size.
                if data.len() as u64 / 4 * 3 > MAX_IMAGE_BYTES {
                    return Err(format!(
                        "the clipboard image is over {} MB as PNG",
                        MAX_IMAGE_BYTES / (1024 * 1024)
                    ));
                }
            }
            Ok(Some(block))
        }
        Err(arboard::Error::ContentNotAvailable) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_file(dir: &Path, name: &str) -> PathBuf {
        let block = png_block(1, 1, &[255, 0, 0, 255]).unwrap();
        let ContentBlock::Image { data, .. } = block else {
            panic!("image");
        };
        let path = dir.join(name);
        std::fs::write(
            &path,
            base64::engine::general_purpose::STANDARD
                .decode(data)
                .unwrap(),
        )
        .unwrap();
        path
    }

    #[test]
    fn only_images_whose_chip_is_still_in_the_text_are_sent() {
        let mut attachments = Attachments::default();
        let image = |n: &str| ContentBlock::Image {
            data: n.into(),
            mime_type: "image/png".into(),
        };
        let first = attachments.add(image("a"));
        let second = attachments.add(image("b"));
        assert_eq!(first, "[image #1]");
        assert_eq!(second, "[image #2]");
        let sent = attachments.take_used(&format!("look at {second}"));
        assert_eq!(sent, vec![image("b")]);
        assert!(attachments.take_used(&second).is_empty(), "sent once");
        assert_eq!(attachments.add(image("c")), "[image #3]");
    }

    #[test]
    fn an_image_file_becomes_a_content_block() {
        let dir = tempfile::tempdir().unwrap();
        png_file(dir.path(), "shot.png");
        let block = image_file(dir.path(), Path::new("shot.png")).unwrap();
        assert!(
            matches!(block, ContentBlock::Image { ref mime_type, .. } if mime_type == "image/png")
        );
        std::fs::write(dir.path().join("notes.txt"), "x").unwrap();
        assert!(image_file(dir.path(), Path::new("notes.txt")).is_err());
        assert!(image_file(dir.path(), Path::new("missing.png")).is_err());
    }

    #[test]
    fn a_pasted_path_to_an_image_is_recognised() {
        let dir = tempfile::tempdir().unwrap();
        let path = png_file(dir.path(), "my shot.png");
        let shown = path.to_string_lossy().to_string();
        for pasted in [
            shown.clone(),
            format!("'{shown}'"),
            format!("\"{shown}\"  "),
            shown.replace(' ', "\\ "),
        ] {
            assert_eq!(
                pasted_image_path(dir.path(), &pasted).as_deref(),
                Some(path.as_path()),
                "{pasted}"
            );
        }
        assert_eq!(
            pasted_image_path(dir.path(), "my shot.png"),
            Some(PathBuf::from("my shot.png")),
            "relative to the working directory"
        );
        assert_eq!(pasted_image_path(dir.path(), "hello world"), None);
        assert_eq!(pasted_image_path(dir.path(), "missing.png"), None);
    }

    #[test]
    fn at_mentions_of_image_files_are_found() {
        let dir = tempfile::tempdir().unwrap();
        png_file(dir.path(), "a.png");
        std::fs::write(dir.path().join("b.rs"), "").unwrap();
        assert_eq!(
            mentioned_images(dir.path(), "compare @a.png with @b.rs and @gone.png"),
            vec![PathBuf::from("a.png")]
        );
    }
}
