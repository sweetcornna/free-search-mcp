//! Images are described, not transcribed (`_parse_blob` image branch):
//! `doc.read` refuses them and points at `web.fetch`, naming the format and
//! dimensions `imagesize` reads from the header.

use crate::markdown::human_bytes;
use fsm_core::FsmError;

/// `"PNG, 640×480, 12.0 KB"`-style description, when the header parses.
pub(crate) fn describe(blob: &[u8]) -> Option<String> {
    let kind = imagesize::image_type(blob).ok()?;
    let size = imagesize::blob_size(blob).ok()?;
    Some(format!(
        "{}, {}×{}, {}",
        format!("{kind:?}").to_uppercase(),
        size.width,
        size.height,
        human_bytes(blob.len() as u64)
    ))
}

/// The error `doc.read` / `doc.outline` return for an image.
pub(crate) fn image_error(source: &str, blob: &[u8]) -> FsmError {
    let what = describe(blob).map_or_else(String::new, |d| format!(" ({d})"));
    FsmError::invalid(format!(
        "{source:?} is an image{what}; doc.read extracts text and an image has none. Use \
         `web.fetch(url)` to get its dimensions, format and size, or \
         `web.fetch(url, {{inlineImage: true}})` to pass the image itself to a vision-capable model."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(w: u32, h: u32) -> Vec<u8> {
        let mut v = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
        v.extend(w.to_be_bytes());
        v.extend(h.to_be_bytes());
        v.extend([8, 6, 0, 0, 0, 0, 0, 0, 0]);
        v
    }

    #[test]
    fn describes_dimensions_and_points_at_fetch() {
        let d = describe(&png(640, 480)).unwrap();
        assert!(d.starts_with("PNG, 640×480"), "{d}");
        let msg = image_error("p.png", &png(2, 3)).to_string();
        assert!(
            msg.contains("Use `web.fetch") && msg.contains("2×3"),
            "{msg}"
        );
        let bare = image_error("x.png", b"\x89PNG\r\n\x1a\n").to_string();
        assert!(bare.contains("is an image;"), "{bare}");
    }
}
