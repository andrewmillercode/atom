//! read_file tool + normalizeImage, ported from main.go executeToolFor
//! ("read_file") and preview.go normalizeImage.

use crate::file_edit;
use crate::{ToolCtx, ToolOutcome};
use atom_core::types::ImageData;
use atom_core::util::sha256_hash;
use base64::Engine;
use std::io::Cursor;

/// executeReadFile reads the file, records it as seen for later
/// write/edit drift checks, and returns image attachments for image
/// files so vision-capable models can see them.
pub fn execute_read_file(arguments: &str, ctx: &ToolCtx<'_>) -> ToolOutcome {
    #[derive(serde::Deserialize, Default)]
    struct Args {
        #[serde(default)]
        path: String,
        #[serde(default)]
        offset: i64,
        #[serde(default)]
        limit: i64,
    }
    let args: Args = match serde_json::from_str(arguments) {
        Ok(a) => a,
        Err(e) => {
            return ToolOutcome {
                text: format!("error parsing arguments: {e}"),
                ..Default::default()
            }
        }
    };
    let content = match std::fs::read(&args.path) {
        Ok(c) => c,
        Err(e) => {
            return ToolOutcome {
                text: format!("error reading file: {e}"),
                ..Default::default()
            }
        }
    };
    file_edit::remember_file(ctx, &args.path, &content);
    if sniff_image_mime(&content).is_some() {
        if content.len() > atom_core::types::MAX_IMAGE_SOURCE_BYTES {
            return ToolOutcome {
                text: format!(
                    "error: image {} is {} bytes, larger than the {}-byte limit for reading images",
                    args.path,
                    content.len(),
                    atom_core::types::MAX_IMAGE_SOURCE_BYTES
                ),
                ..Default::default()
            };
        }
        let (out, out_mime) = match normalize_image(&content) {
            Ok(v) => v,
            Err(e) => {
                return ToolOutcome {
                    text: format!("error: cannot attach image {}: {}", args.path, e),
                    ..Default::default()
                }
            }
        };
        let img = ImageData {
            mime: out_mime,
            data: base64::engine::general_purpose::STANDARD.encode(&out),
        };
        return ToolOutcome {
            text: format!("Image file: {} ({} bytes)", args.path, out.len()),
            images: vec![img],
            diff: String::new(),
        };
    }
    let text = String::from_utf8_lossy(&content).to_string();
    ToolOutcome {
        text: file_edit::read_file_output(&text, args.offset, args.limit),
        images: Vec::new(),
        diff: String::new(),
    }
}

// ---------------------------------------------------------------------------
// normalizeImage (preview.go) — fit within maxImageDim, then shrink until
// the base64 payload is at most maxImageBase64Bytes.
// ---------------------------------------------------------------------------

fn b64_len(n: usize) -> usize {
    ((n + 2) / 3) * 4
}

pub(crate) fn normalize_image(data: &[u8]) -> Result<(Vec<u8>, String), String> {
    let mime = sniff_image_mime(data)
        .ok_or_else(|| "unrecognized image format".to_string())?
        .to_string();
    let src = match image::load_from_memory(data) {
        Ok(img) => img,
        Err(e) => {
            // Undecodable payloads pass through when small enough; Go does
            // the same so odd-but-recognizable formats still attach.
            if b64_len(data.len()) <= atom_core::types::MAX_IMAGE_BASE64_BYTES {
                return Ok((data.to_vec(), mime));
            }
            return Err(e.to_string());
        }
    };
    let (w, h) = (src.width() as i64, src.height() as i64);
    if w <= atom_core::types::MAX_IMAGE_DIM as i64
        && h <= atom_core::types::MAX_IMAGE_DIM as i64
        && b64_len(data.len()) <= atom_core::types::MAX_IMAGE_BASE64_BYTES
    {
        return Ok((data.to_vec(), mime));
    }
    let mut scale = 1.0f64;
    if w > atom_core::types::MAX_IMAGE_DIM as i64 || h > atom_core::types::MAX_IMAGE_DIM as i64 {
        let sw = atom_core::types::MAX_IMAGE_DIM as f64 / w as f64;
        let sh = atom_core::types::MAX_IMAGE_DIM as f64 / h as f64;
        scale = sw.min(sh);
    }
    loop {
        let mut nw = (w as f64 * scale).round() as i64;
        let mut nh = (h as f64 * scale).round() as i64;
        if nw < 1 {
            nw = 1;
        }
        if nh < 1 {
            nh = 1;
        }
        let dst = if nw != w || nh != h {
            image::DynamicImage::ImageRgba8(image::imageops::resize(
                &src,
                nw as u32,
                nh as u32,
                image::imageops::FilterType::Triangle,
            ))
        } else {
            src.clone()
        };
        let mut out = Vec::new();
        let mut out_mime = "image/png".to_string();
        dst.write_to(&mut Cursor::new(&mut out), image::ImageFormat::Png)
            .map_err(|e| e.to_string())?;
        if b64_len(out.len()) > atom_core::types::MAX_IMAGE_BASE64_BYTES {
            let mut jpeg = Vec::new();
            let mut cursor = Cursor::new(&mut jpeg);
            let enc = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut cursor, 80);
            if dst.to_rgb8().write_with_encoder(enc).is_ok() {
                out = jpeg;
                out_mime = "image/jpeg".to_string();
            } else {
                out.clear();
            }
        }
        if b64_len(out.len()) <= atom_core::types::MAX_IMAGE_BASE64_BYTES && !out.is_empty() {
            return Ok((out, out_mime));
        }
        scale *= 0.8;
        if nw <= 32 || nh <= 32 {
            return Err("image too large after resize".to_string());
        }
    }
}

fn sniff_image_mime(data: &[u8]) -> Option<&'static str> {
    let s = atom_core::render::diff::sniff_image_mime(data);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// sha256 of file content is only used via FileSeen; exposed here so the
/// read path can assert stable hashes in tests.
pub fn content_hash(data: &[u8]) -> String {
    sha256_hash(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::test_support::*;

    const PNG_MAGIC: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];

    #[test]
    fn window_read_reports_remaining_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("notes.txt");
        let full: String = (1..=8).map(|i| format!("{}\n", "x".repeat(i))).collect();
        std::fs::write(&path, &full).unwrap();

        let ctx = test_ctx(dir.path());
        let out = execute_read_file(
            &serde_json::json!({"path": path.display().to_string(), "offset": 3, "limit": 2})
                .to_string(),
            &ctx,
        );
        assert!(out.images.is_empty() && out.diff.is_empty());
        assert!(!out.text.contains("hash:"), "{}", out.text);
        assert!(out.text.starts_with("xxxx\nxxxxx\n"), "{}", out.text);
        assert!(
            out.text
                .contains("[3 more lines. Use offset=5 to continue.]"),
            "{}",
            out.text
        );

        let out = execute_read_file(
            &serde_json::json!({"path": path.display().to_string()}).to_string(),
            &ctx,
        );
        assert_eq!(out.text, full);
    }

    #[test]
    fn image_files_return_attachments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("tiny.png");
        let mut data = PNG_MAGIC.to_vec();
        data.extend_from_slice(&[1, 2, 3]);
        std::fs::write(&path, &data).unwrap();

        let ctx = test_ctx(dir.path());
        let out = execute_read_file(
            &serde_json::json!({"path": path.display().to_string()}).to_string(),
            &ctx,
        );
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0].mime, "image/png");
        assert_eq!(
            out.text,
            format!("Image file: {} ({} bytes)", path.display(), data.len())
        );
    }

    #[test]
    fn missing_file_error_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let ctx = test_ctx(dir.path());
        let out = execute_read_file(r#"{"path":"/no/such/file"}"#, &ctx);
        assert!(out.text.starts_with("error reading file:"), "{}", out.text);
    }

    #[test]
    fn normalize_passthrough_small_undecodable_png() {
        let mut data = PNG_MAGIC.to_vec();
        data.extend_from_slice(&[9; 3]);
        let (out, mime) = normalize_image(&data).unwrap();
        assert_eq!(mime, "image/png");
        assert_eq!(out, data);
    }

    #[test]
    fn oversized_image_is_scaled_down() {
        // Build a real large PNG (solid color) and force rescaling by
        // temporarily using a tiny dimension cap through direct calls —
        // instead just verify a moderately sized image round-trips.
        let img = image::RgbaImage::from_pixel(3000, 2000, [255u8; 4].into());
        let mut buf = Vec::new();
        image::DynamicImage::ImageRgba8(img.into())
            .write_to(&mut Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        let (out, mime) = normalize_image(&buf).unwrap();
        assert_eq!(mime, "image/png");
        assert!(b64_len(out.len()) <= atom_core::types::MAX_IMAGE_BASE64_BYTES);
        let decoded = image::load_from_memory(&out).unwrap();
        assert!(decoded.width() <= atom_core::types::MAX_IMAGE_DIM);
        assert!(decoded.height() <= atom_core::types::MAX_IMAGE_DIM);
    }
}
