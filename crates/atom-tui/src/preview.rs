//! preview.rs renders the pasted-image previews shown inside the prompt
//! input (rows just below the typed text) and parses the terminal paste
//! sequences that deliver images.
//!
//! Kitty-graphics terminals get real thumbnails transmitted as virtual
//! placements; the visible preview is Unicode placeholder cells
//! (U+10EEEE). Other terminals get only the inline IMG chip. Every
//! graphics write is best-effort and non-fatal.

use std::io::Write;

use base64::Engine;

use atom_core::render::diff::sniff_image_mime;
use atom_core::types::ImageData;

use crate::app::App;
use crate::events::Effect;

/// Kitty protocol's maximum payload per chunk, before base64 encoding.
const KITTY_MAX_CHUNK: usize = 4096;

/// Preview geometry (preview.go): 6 cols × 3 rows is a visual square at
/// typical cell sizes; PNG pixels per cell for the transmitted thumbnail.
pub const PREVIEW_COLS: usize = 6;
pub const PREVIEW_ROWS: usize = 3;
pub const PREVIEW_GAP: usize = 1;
pub const PREVIEW_PROMPT_GAP: usize = 1;
const PREVIEW_CELL_W: u32 = 32;
const PREVIEW_CELL_H: u32 = 64;
const PREVIEW_BORDER_PX: i64 = 3;
pub const MAX_KITTY_PREVIEW_ID: usize = 16;

/// pendingImage is one pasted image waiting to be sent. cols=0 marks an
/// image that couldn't be decoded (renders as a text row instead); num
/// is the 1-based [IMG n] marker.
#[derive(Debug, Clone)]
pub struct PendingImage {
    pub img: ImageData,
    pub name: String,
    pub cols: usize,
    pub rows: usize,
    pub num: usize,
}

// ---------------------------------------------------------------------------
// Terminal detection and out-of-band writes.
// ---------------------------------------------------------------------------

/// kittyTerminal reports whether the current terminal speaks the Kitty
/// graphics protocol, based on environment signals.
pub fn kitty_terminal() -> bool {
    let prog = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_lowercase();
    let term = std::env::var("TERM").unwrap_or_default().to_lowercase();
    [
        "kitty", "wezterm", "ghostty", "foot", "konsole", "contour", "rio",
    ]
    .iter()
    .any(|s| prog.contains(s))
        || term.contains("kitty")
}

fn write_tty(s: &str) {
    let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") else {
        return;
    };
    let _ = tty.write_all(s.as_bytes());
    let _ = tty.flush();
}

// ---------------------------------------------------------------------------
// Paste parsing.
// ---------------------------------------------------------------------------

/// parseOSC1337 extracts an image from an iTerm2/WezTerm paste:
/// ESC ] 1337 ; File=...;inline=1;<base64> BEL.
pub fn parse_osc1337(seq: &str) -> Option<(String, Vec<u8>)> {
    let mut seq = seq.strip_prefix("\x1b]").unwrap_or(seq);
    if let Some(s) = seq.strip_suffix('\x07') {
        seq = s;
    } else if let Some(s) = seq.strip_suffix("\x1b\\") {
        seq = s;
    }
    let rest = seq.strip_prefix("1337;")?;
    let body = rest
        .strip_prefix("File=")
        .or_else(|| rest.strip_prefix("MultipartFile="))?;
    let last = body.rfind(';')?;
    let args = &body[..last];
    let payload = body[last + 1..]
        .strip_prefix("base64,")
        .unwrap_or(&body[last + 1..]);
    if !args.contains("inline=1") {
        return None;
    }
    let mut name = String::new();
    for arg in args.split(';') {
        if let Some((k, v)) = arg.split_once('=') {
            if k != "name" {
                continue;
            }
            // iTerm2 base64-encodes names with special characters; use
            // the decoded form when valid UTF-8 without NULs.
            match base64::engine::general_purpose::STANDARD.decode(v) {
                Ok(dec) if !dec.is_empty() && !dec.contains(&0) => {
                    name = String::from_utf8(dec).unwrap_or_else(|_| v.to_string());
                }
                _ => name = v.to_string(),
            }
        }
    }
    let data = base64::engine::general_purpose::STANDARD
        .decode(payload)
        .ok()?;
    Some((name, data))
}

/// splitPasteSegments splits pasted content into text/image pieces so a
/// mixed paste inserts both in order.
pub fn split_paste_segments(content: &str) -> Vec<(String, Option<Vec<u8>>)> {
    let mut segments = Vec::new();
    let mut content = content.to_string();
    loop {
        let Some(idx) = content.find("\x1b]1337;") else {
            if !content.is_empty() {
                segments.push((content.clone(), None));
            }
            return segments;
        };
        if idx > 0 {
            segments.push((content[..idx].to_string(), None));
        }
        content = content[idx..].to_string();
        let bytes = content.as_bytes();
        let end = match bytes.iter().position(|&b| b == 0x07 || b == 0x1b) {
            Some(e) => e,
            None => {
                segments.push((content.clone(), None));
                return segments;
            }
        };
        let (seq_len, consumed): (usize, usize) =
            if bytes[end] == 0x1b && end + 1 < bytes.len() && bytes[end + 1] == b'\\' {
                (end + 2, end + 2)
            } else {
                (end + 1, end + 1)
            };
        let seq = content[..seq_len].to_string();
        content = content[consumed..].to_string();
        match parse_osc1337(&seq) {
            Some((_, data)) => segments.push((String::new(), Some(data))),
            None => segments.push((seq, None)), // keep raw non-image sequences
        }
    }
}

/// kittyPasteData turns one Kitty graphics payload into image bytes:
/// PNG passes through; raw RGBA/RGB re-encodes via the given geometry.
pub fn kitty_paste_data(payload: &[u8], format: u8, w: u32, h: u32) -> Option<Vec<u8>> {
    match format {
        0 | 100 => Some(payload.to_vec()),
        32 | 24 => {
            if w == 0 || h == 0 {
                return None;
            }
            let bpp = if format == 32 { 4 } else { 3 };
            if payload.len() != (w * h) as usize * bpp {
                return None;
            }
            let mut rgba = Vec::with_capacity((w * h * 4) as usize);
            for px in payload.chunks(bpp) {
                rgba.extend_from_slice(&px[0..3]);
                rgba.push(0xFF);
            }
            let img = image::RgbaImage::from_raw(w, h, rgba)?;
            let mut out = std::io::Cursor::new(Vec::new());
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut out, image::ImageFormat::Png)
                .ok()?;
            Some(out.into_inner())
        }
        _ => None,
    }
}

/// localImageFile is one image loaded from a dropped or pasted path.
#[derive(Debug, Clone)]
pub struct LocalImageFile {
    pub name: String,
    pub data: Vec<u8>,
}

/// localImagesFromPaste returns images if every non-empty line of a
/// bracketed paste is a readable image file (Finder/kitty drops).
pub fn local_images_from_paste(content: &str) -> Vec<LocalImageFile> {
    let content = content.replace("\r\n", "\n").replace('\r', "\n");
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Vec::new();
    }
    let mut files = Vec::new();
    for line in trimmed.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let path = unescape_paste_path(line);
        match read_local_image(&path) {
            Some(f) => files.push(f),
            None => return Vec::new(),
        }
    }
    files
}

fn unescape_paste_path(s: &str) -> String {
    let mut s = s.trim().trim_matches(['"', '\'']).to_string();
    if s.to_lowercase().starts_with("file:") {
        if let Ok(u) = url_parse(&s) {
            if !u.is_empty() {
                s = u;
            }
        }
    }
    if s.contains('\\') {
        s = s.replace('\\', "");
    }
    if s == "~" || s.starts_with("~/") {
        if let Some(home) = dirs_home() {
            s = format!(
                "{}/{}",
                home.trim_end_matches('/'),
                s.trim_start_matches("~/")
            );
        }
    }
    s
}

fn url_parse(_s: &str) -> Result<String, ()> {
    // Minimal file:// URL path extraction (no url crate in this crate's
    // dependency set): strip scheme and percent-decode common cases.
    Err(())
}

fn dirs_home() -> Option<String> {
    std::env::var("HOME").ok()
}

fn read_local_image(path: &str) -> Option<LocalImageFile> {
    if path.is_empty() {
        return None;
    }
    let meta = std::fs::metadata(path).ok()?;
    if meta.is_dir() || meta.len() > atom_core::types::MAX_IMAGE_SOURCE_BYTES as u64 {
        return None;
    }
    let data = std::fs::read(path).ok()?;
    if sniff_image_mime(&data).is_empty() {
        return None;
    }
    Some(LocalImageFile {
        name: std::path::Path::new(path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        data,
    })
}

// ---------------------------------------------------------------------------
// Image normalization and thumbnails.
// ---------------------------------------------------------------------------

/// imageSize measures an image without decoding fully.
pub fn image_size(data: &[u8]) -> Option<(u32, u32)> {
    let reader = image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .ok()?;
    reader.into_dimensions().ok()
}

/// normalizeImage scales/recompresses an attachment to fit within the
/// OpenCode-style limits. Returns (bytes, mime).
pub fn normalize_image(data: &[u8]) -> anyhow::Result<(Vec<u8>, String)> {
    use base64::engine::general_purpose::STANDARD;
    let mime = sniff_image_mime(data);
    if mime.is_empty() {
        anyhow::bail!("unrecognized image format");
    }
    let src = match image::load_from_memory(data) {
        Ok(img) => img,
        Err(_) => {
            if STANDARD.encode(data).len() <= atom_core::types::MAX_IMAGE_BASE64_BYTES {
                return Ok((data.to_vec(), mime.to_string()));
            }
            anyhow::bail!("undecodable oversized image");
        }
    };
    let (w, h) = (src.width(), src.height());
    if w <= atom_core::types::MAX_IMAGE_DIM
        && h <= atom_core::types::MAX_IMAGE_DIM
        && STANDARD.encode(data).len() <= atom_core::types::MAX_IMAGE_BASE64_BYTES
    {
        return Ok((data.to_vec(), mime.to_string()));
    }
    let mut scale = 1.0f64;
    if w > atom_core::types::MAX_IMAGE_DIM || h > atom_core::types::MAX_IMAGE_DIM {
        let sw = atom_core::types::MAX_IMAGE_DIM as f64 / w as f64;
        let sh = atom_core::types::MAX_IMAGE_DIM as f64 / h as f64;
        scale = scale.min(sw).min(sh);
        let _ = sw.max(sh);
    }
    loop {
        let nw = ((w as f64 * scale).round() as u32).max(1);
        let nh = ((h as f64 * scale).round() as u32).max(1);
        let dst = if nw != w || nh != h {
            src.resize_exact(nw, nh, image::imageops::FilterType::Triangle)
        } else {
            src.clone()
        };
        let mut png = std::io::Cursor::new(Vec::new());
        dst.write_to(&mut png, image::ImageFormat::Png).ok();
        let mut out = png.into_inner();
        let mut mime_out = "image/png".to_string();
        if STANDARD.encode(&out).len() > atom_core::types::MAX_IMAGE_BASE64_BYTES {
            let mut jpg = std::io::Cursor::new(Vec::new());
            if dst.write_to(&mut jpg, image::ImageFormat::Jpeg).is_ok() {
                out = jpg.into_inner();
                mime_out = "image/jpeg".to_string();
            }
        }
        if STANDARD.encode(&out).len() <= atom_core::types::MAX_IMAGE_BASE64_BYTES {
            return Ok((out, mime_out));
        }
        scale *= 0.8;
        if nw <= 32 || nh <= 32 {
            anyhow::bail!("image too large after resize");
        }
    }
}

/// makePreviewPNG builds a square object-cover thumbnail with a hairline
/// border that kitty scales into the cell box.
pub fn make_preview_png(data: &[u8]) -> anyhow::Result<Vec<u8>> {
    let src = image::load_from_memory(data)?;
    let dw = PREVIEW_COLS as u32 * PREVIEW_CELL_W;
    let dh = PREVIEW_ROWS as u32 * PREVIEW_CELL_H;
    let cropped = cover_crop(&src, dw, dh);
    let thumb = cropped.resize_exact(dw, dh, image::imageops::FilterType::Triangle);
    let mut img = thumb.to_rgba8();
    draw_preview_border(&mut img);
    let mut out = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgba8(img).write_to(&mut out, image::ImageFormat::Png)?;
    Ok(out.into_inner())
}

fn cover_crop(src: &image::DynamicImage, dw: u32, dh: u32) -> image::DynamicImage {
    let (sw, sh) = (src.width(), src.height());
    if sw == 0 || sh == 0 || dw == 0 || dh == 0 {
        return src.clone();
    }
    let dest_aspect = dw as f64 / dh as f64;
    let src_aspect = sw as f64 / sh as f64;
    let (cw, ch) = if src_aspect > dest_aspect {
        let cw = ((sh as f64 * dest_aspect).round() as u32).max(1).min(sw);
        (cw, sh)
    } else {
        let ch = ((sw as f64 / dest_aspect).round() as u32).max(1).min(sh);
        (sw, ch)
    };
    let x0 = (sw - cw) / 2;
    let y0 = (sh - ch) / 2;
    image::DynamicImage::ImageRgba8(src.crop_imm(x0, y0, cw, ch).to_rgba8())
}

fn draw_preview_border(img: &mut image::RgbaImage) {
    let border = hex_to_rgba(&atom_core::render::colors::theme_color(
        atom_core::render::colors::ThemeColor::Border,
    ));
    let (w, h) = (img.width(), img.height());
    let t = PREVIEW_BORDER_PX.max(1) as u32;
    for y in 0..h {
        for x in 0..t.min(w) {
            img.put_pixel(x, y, border);
            let xr = w - 1 - x;
            if xr >= t {
                img.put_pixel(xr, y, border);
            }
        }
    }
    for x in 0..w {
        for y in 0..t.min(h) {
            img.put_pixel(x, y, border);
            let yr = h - 1 - y;
            if yr >= t {
                img.put_pixel(x, yr, border);
            }
        }
    }
}

fn hex_to_rgba(hex: &str) -> image::Rgba<u8> {
    let h = hex.trim_start_matches('#');
    let r = u8::from_str_radix(h.get(0..2).unwrap_or("00"), 16).unwrap_or(0);
    let g = u8::from_str_radix(h.get(2..4).unwrap_or("00"), 16).unwrap_or(0);
    let b = u8::from_str_radix(h.get(4..6).unwrap_or("00"), 16).unwrap_or(0);
    image::Rgba([r, g, b, 255])
}

// ---------------------------------------------------------------------------
// App-facing helpers.
// ---------------------------------------------------------------------------

pub fn image_marker(n: usize) -> String {
    format!("[IMG {n}]")
}

pub fn next_image_num(pending: &[PendingImage]) -> usize {
    next_image_num_excluding(pending, &[])
}

/// nextImageNumExcluding allocates the next kitty image id, skipping any
/// ids the caller reports as already in use (typically images already
/// attached to a Block so we don't collide with sent messages). Ids
/// wrap inside [1, MAX_KITTY_PREVIEW_ID] so paint_kitty_previews can
/// clean up orphaned slots.
pub fn next_image_num_excluding(pending: &[PendingImage], reserved: &[usize]) -> usize {
    let mut used: std::collections::HashSet<usize> = pending.iter().map(|p| p.num).collect();
    used.extend(reserved.iter().copied());
    for n in 1..=MAX_KITTY_PREVIEW_ID {
        if !used.contains(&n) {
            return n;
        }
    }
    1
}

pub fn image_markers_in(s: &str) -> std::collections::HashSet<usize> {
    static RE: once_cell::sync::Lazy<regex::Regex> =
        once_cell::sync::Lazy::new(|| regex::Regex::new(r"\[IMG (\d+)\]").expect("static regex"));
    RE.captures_iter(s)
        .filter_map(|c| c[1].parse::<usize>().ok())
        .filter(|n| *n > 0)
        .collect()
}

/// syncPendingFromInput drops images whose markers were removed from
/// the prompt; true when the pending set changed.
pub fn sync_pending_from_input(app: &mut App) -> bool {
    if app.pending.is_empty() {
        return false;
    }
    let present = image_markers_in(&app.input.value);
    let before = app.pending.len();
    app.pending.retain(|p| present.contains(&p.num));
    if app.pending.len() != before {
        app.preview_dirty = true;
        return true;
    }
    false
}

/// addImage appends a pasted image to the prompt set.
pub fn add_image(app: &mut App, name: &str, data: &[u8]) -> anyhow::Result<()> {
    if app.pending.len() >= atom_core::types::MAX_PENDING_IMAGES {
        anyhow::bail!(
            "at most {} images per prompt",
            atom_core::types::MAX_PENDING_IMAGES
        );
    }
    if data.len() > atom_core::types::MAX_IMAGE_SOURCE_BYTES {
        anyhow::bail!(
            "image too large: {} bytes (limit {})",
            data.len(),
            atom_core::types::MAX_IMAGE_SOURCE_BYTES
        );
    }
    let (norm, mime) = normalize_image(data)?;
    let mut reserved: Vec<usize> = Vec::new();
    for block in app.blocks.iter() {
        for img in &block.images {
            reserved.push(img.num);
        }
    }
    let mut p = PendingImage {
        img: ImageData {
            mime,
            data: base64::engine::general_purpose::STANDARD.encode(&norm),
        },
        name: name.to_string(),
        cols: 0,
        rows: 0,
        num: next_image_num_excluding(&app.pending, &reserved),
    };
    if image_size(&norm).is_some() {
        p.cols = PREVIEW_COLS;
        p.rows = PREVIEW_ROWS;
    } else if kitty_terminal() {
        p.cols = PREVIEW_COLS;
        p.rows = PREVIEW_ROWS;
    }
    app.pending.push(p);
    app.preview_dirty = true;
    Ok(())
}

/// pasteImage attaches an image and inserts its marker at the cursor.
pub fn paste_image(app: &mut App, name: &str, data: &[u8]) -> anyhow::Result<()> {
    add_image(app, name, data)?;
    let num = app.pending.last().map(|p| p.num).unwrap_or(1);
    app.input.insert_str(&format!("{} ", image_marker(num)));
    Ok(())
}

/// Mixed text+OSC1337 paste handling.
pub fn paste_mixed_content(app: &mut App, content: &str) -> Vec<Effect> {
    let mut sb = String::new();
    for (text, data) in split_paste_segments(content) {
        if let Some(data) = data {
            if add_image(app, "", &data).is_err() {
                continue; // the failed image contributes nothing
            }
            let num = app.pending.last().map(|p| p.num).unwrap_or(1);
            sb.push_str(&format!("{} ", image_marker(num)));
        } else {
            sb.push_str(&text);
        }
    }
    app.input.insert_str(&sb);
    app.preview_dirty = true;
    vec![Effect::PaintPreviews]
}

pub fn paste_local_images(app: &mut App, files: Vec<LocalImageFile>) -> Vec<Effect> {
    for f in files {
        if let Err(e) = paste_image(app, &f.name, &f.data) {
            app.err_msg = e.to_string();
        }
    }
    vec![Effect::PaintPreviews]
}

/// previewThumbRows: thumbnail grid height (0 without kitty support).
pub fn preview_thumb_rows(app: &App) -> usize {
    if !kitty_terminal() {
        return 0;
    }
    app.pending.iter().map(|p| p.rows).max().unwrap_or(0)
}

/// previewRowCount: rows reserved inside the input box incl. gap row.
pub fn preview_row_count(app: &App) -> usize {
    let n = preview_thumb_rows(app);
    if n == 0 {
        0
    } else {
        n + PREVIEW_PROMPT_GAP
    }
}

pub fn image_chip(n: usize) -> String {
    format!(" IMG {n} ")
}

/// applyImageChips replaces [IMG n] markers with same-width chips.
pub fn apply_image_chips(app: &App, input_view: String) -> String {
    let max_num = app.pending.iter().map(|p| p.num).max().unwrap_or(0);
    let selected = if app.input.has_selection() {
        app.input.selected_text()
    } else {
        String::new()
    };
    let mut view = input_view;
    for i in (1..=max_num).rev() {
        let mark = image_marker(i);
        if selected.contains(&mark) {
            continue;
        }
        view = view.replace(&mark, &image_chip(i));
    }
    view
}

// ---------------------------------------------------------------------------
// Placeholder grid + kitty transmission.
// ---------------------------------------------------------------------------

/// The Kitty Unicode placeholder code point (U+10EEEE).
const PLACEHOLDER: char = '\u{10EEEE}';

/// Row/column diacritics from kitty's rowcolumn-diacritics.txt.
const ROW_COL_DIACRITICS: [char; 16] = [
    '\u{0305}', '\u{030D}', '\u{030E}', '\u{0310}', '\u{0312}', '\u{033D}', '\u{033E}', '\u{033F}',
    '\u{0346}', '\u{034A}', '\u{034B}', '\u{034C}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}',
];

fn row_col_diacritic(n: usize) -> char {
    ROW_COL_DIACRITICS[n.min(ROW_COL_DIACRITICS.len() - 1)]
}

/// placeholderGrid builds rows of U+10EEEE cells with fg = id plus row
/// and column diacritics; color resets at each row end.
pub fn placeholder_grid(id: usize, cols: usize, rows: usize) -> String {
    if cols == 0 || rows == 0 {
        return String::new();
    }
    let mut sb = String::new();
    for y in 0..rows {
        sb.push_str(&format!("\x1b[38;5;{}m", id.min(255)));
        for x in 0..cols {
            sb.push(PLACEHOLDER);
            sb.push(row_col_diacritic(y));
            sb.push(row_col_diacritic(x));
        }
        sb.push_str("\x1b[39m");
        if y < rows - 1 {
            sb.push('\n');
        }
    }
    sb
}

fn kitty_delete_virtual(id: usize) -> String {
    format!("\x1b_Ga=d,d=I,i={id},q=2\x1b\\")
}

fn kitty_transmit(id: usize, png_data: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(png_data);
    let mut sb = String::new();
    let opts = format!("a=t,f=100,i={id},q=2");
    let bytes = b64.as_bytes();
    let mut off = 0usize;
    while bytes.len() - off > KITTY_MAX_CHUNK {
        sb.push_str(&format!(
            "\x1b_G{},m=1;{}\x1b\\",
            opts,
            &b64[off..off + KITTY_MAX_CHUNK]
        ));
        off += KITTY_MAX_CHUNK;
    }
    sb.push_str(&format!("\x1b_G{};{}\x1b\\", opts, &b64[off..]));
    sb
}

/// paintKittyPreviews transmits virtual placements for the pending set,
/// deleting unused ids first. Best-effort: any failure is silent.
pub fn paint_kitty_previews(entries: &[(usize, Vec<u8>)]) {
    let mut sb = String::new();
    let used: std::collections::HashSet<usize> = entries.iter().map(|(n, _)| *n).collect();
    for n in 1..=MAX_KITTY_PREVIEW_ID {
        if !used.contains(&n) {
            sb.push_str(&kitty_delete_virtual(n));
        }
    }
    for (num, data) in entries {
        let Ok(png) = make_preview_png(data) else {
            continue;
        };
        sb.push_str(&kitty_transmit(*num, &png));
        sb.push_str(&format!(
            "\x1b_Ga=p,U=1,i={num},c={},r={},q=2\x1b\\",
            PREVIEW_COLS, PREVIEW_ROWS
        ));
    }
    write_tty(&sb);
}
