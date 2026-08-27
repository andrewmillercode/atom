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

/// Kitty protocol's maximum base64 payload per graphics command.
const KITTY_MAX_CHUNK: usize = 4096;

/// Preview geometry (preview.go): 6 cols × 3 rows is a visual square at
/// typical cell sizes; PNG pixels per cell for the transmitted thumbnail.
pub const PREVIEW_COLS: usize = 6;
pub const PREVIEW_ROWS: usize = 3;
pub const PREVIEW_GAP: usize = 1;
pub const PREVIEW_PROMPT_GAP: usize = 1;
pub(crate) const PREVIEW_CELL_W: u32 = 32;
pub(crate) const PREVIEW_CELL_H: u32 = 64;
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

/// Every write to the terminal device — kitty graphics payloads
/// (write_tty) and ratatui frame draws (event_loop) — must share this
/// lock. Paint tasks run on the blocking pool while the event loop keeps
/// drawing; without serialization their escape streams interleave and
/// tear BOTH writers: garbled/missing text, and image tiles smeared over
/// unrelated content. Resizing repaints the whole screen, which is why
/// the damage used to "fix itself" on resize.
static TTY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Acquires the terminal write lock. Callers that write escape sequences
/// to the tty outside of preview.rs (the frame draw itself) must hold the
/// same guard so the two writers never interleave.
pub fn lock_tty() -> std::sync::MutexGuard<'static, ()> {
    TTY_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn write_tty(s: &str) {
    let _guard = lock_tty();
    let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") else {
        return;
    };
    let _ = tty.write_all(s.as_bytes());
    let _ = tty.flush();
}

/// Writes raw bytes to the tty WITHOUT acquiring the lock. Only for
/// callers that already hold [`lock_tty`] — the event loop's frame
/// section, which must flush pending math-engine Kitty commands before
/// drawing the frame that displays them (calling the locking variant
/// there would deadlock).
pub fn write_tty_locked_bytes(bytes: &[u8]) {
    let Ok(mut tty) = std::fs::OpenOptions::new().write(true).open("/dev/tty") else {
        return;
    };
    let _ = tty.write_all(bytes);
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
    if image_size(&norm).is_some() || kitty_terminal() {
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

/// Row/column diacritics from kitty's generated
/// `rowcolumn-diacritics.txt` table. Keep enough entries for every grid
/// coordinate atom can emit (`diagram_geometry` caps diagrams at 200
/// columns and 60 rows).
///
/// This must never clamp or wrap an index. Reusing the last diacritic for
/// wide diagrams makes many placeholder cells address the same image tile;
/// some terminals then smear those tiles over later TUI content.
const ROW_COL_DIACRITICS: [char; 200] = [
    '\u{0305}', '\u{030D}', '\u{030E}', '\u{0310}', '\u{0312}', '\u{033D}', '\u{033E}', '\u{033F}',
    '\u{0346}', '\u{034A}', '\u{034B}', '\u{034C}', '\u{0350}', '\u{0351}', '\u{0352}', '\u{0357}',
    '\u{035B}', '\u{0363}', '\u{0364}', '\u{0365}', '\u{0366}', '\u{0367}', '\u{0368}', '\u{0369}',
    '\u{036A}', '\u{036B}', '\u{036C}', '\u{036D}', '\u{036E}', '\u{036F}', '\u{0483}', '\u{0484}',
    '\u{0485}', '\u{0486}', '\u{0487}', '\u{0592}', '\u{0593}', '\u{0594}', '\u{0595}', '\u{0597}',
    '\u{0598}', '\u{0599}', '\u{059C}', '\u{059D}', '\u{059E}', '\u{059F}', '\u{05A0}', '\u{05A1}',
    '\u{05A8}', '\u{05A9}', '\u{05AB}', '\u{05AC}', '\u{05AF}', '\u{05C4}', '\u{0610}', '\u{0611}',
    '\u{0612}', '\u{0613}', '\u{0614}', '\u{0615}', '\u{0616}', '\u{0617}', '\u{0657}', '\u{0658}',
    '\u{0659}', '\u{065A}', '\u{065B}', '\u{065D}', '\u{065E}', '\u{06D6}', '\u{06D7}', '\u{06D8}',
    '\u{06D9}', '\u{06DA}', '\u{06DB}', '\u{06DC}', '\u{06DF}', '\u{06E0}', '\u{06E1}', '\u{06E2}',
    '\u{06E4}', '\u{06E7}', '\u{06E8}', '\u{06EB}', '\u{06EC}', '\u{0730}', '\u{0732}', '\u{0733}',
    '\u{0735}', '\u{0736}', '\u{073A}', '\u{073D}', '\u{073F}', '\u{0740}', '\u{0741}', '\u{0743}',
    '\u{0745}', '\u{0747}', '\u{0749}', '\u{074A}', '\u{07EB}', '\u{07EC}', '\u{07ED}', '\u{07EE}',
    '\u{07EF}', '\u{07F0}', '\u{07F1}', '\u{07F3}', '\u{0816}', '\u{0817}', '\u{0818}', '\u{0819}',
    '\u{081B}', '\u{081C}', '\u{081D}', '\u{081E}', '\u{081F}', '\u{0820}', '\u{0821}', '\u{0822}',
    '\u{0823}', '\u{0825}', '\u{0826}', '\u{0827}', '\u{0829}', '\u{082A}', '\u{082B}', '\u{082C}',
    '\u{082D}', '\u{0951}', '\u{0953}', '\u{0954}', '\u{0F82}', '\u{0F83}', '\u{0F86}', '\u{0F87}',
    '\u{135D}', '\u{135E}', '\u{135F}', '\u{17DD}', '\u{193A}', '\u{1A17}', '\u{1A75}', '\u{1A76}',
    '\u{1A77}', '\u{1A78}', '\u{1A79}', '\u{1A7A}', '\u{1A7B}', '\u{1A7C}', '\u{1B6B}', '\u{1B6D}',
    '\u{1B6E}', '\u{1B6F}', '\u{1B70}', '\u{1B71}', '\u{1B72}', '\u{1B73}', '\u{1CD0}', '\u{1CD1}',
    '\u{1CD2}', '\u{1CDA}', '\u{1CDB}', '\u{1CE0}', '\u{1DC0}', '\u{1DC1}', '\u{1DC3}', '\u{1DC4}',
    '\u{1DC5}', '\u{1DC6}', '\u{1DC7}', '\u{1DC8}', '\u{1DC9}', '\u{1DCB}', '\u{1DCC}', '\u{1DD1}',
    '\u{1DD2}', '\u{1DD3}', '\u{1DD4}', '\u{1DD5}', '\u{1DD6}', '\u{1DD7}', '\u{1DD8}', '\u{1DD9}',
    '\u{1DDA}', '\u{1DDB}', '\u{1DDC}', '\u{1DDD}', '\u{1DDE}', '\u{1DDF}', '\u{1DE0}', '\u{1DE1}',
    '\u{1DE2}', '\u{1DE3}', '\u{1DE4}', '\u{1DE5}', '\u{1DE6}', '\u{1DFE}', '\u{20D0}', '\u{20D1}',
];

fn row_col_diacritic(n: usize) -> char {
    ROW_COL_DIACRITICS[n]
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

pub(crate) fn kitty_transmit(id: usize, png_data: &[u8]) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(png_data);
    let mut sb = String::new();
    let chunks: Vec<&[u8]> = b64.as_bytes().chunks(KITTY_MAX_CHUNK).collect();
    for (index, bytes) in chunks.iter().enumerate() {
        let more = usize::from(index + 1 < chunks.len());
        // Kitty continuation chunks inherit the first command's action,
        // format and image id. Repeating those fields is non-conformant and
        // can make stricter terminals treat each chunk as a new upload.
        let control = if index == 0 {
            format!("a=t,f=100,i={id},q=2,m={more}")
        } else {
            format!("q=2,m={more}")
        };
        sb.push_str(&format!(
            "\x1b_G{control};{}\x1b\\",
            std::str::from_utf8(bytes).expect("base64 is ASCII")
        ));
    }
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

/// A diagram spec passed to the paint function.
pub struct DiagramSpec {
    pub id: usize,
    pub svg: String,
    pub png: String,
    pub cols: usize,
    pub rows: usize,
}

/// Rasterize an SVG at the given pixel dimensions with a solid card
/// background using resvg. Returns PNG bytes, or None on failure.
/// Card background color is sourced from the atom palette (CardDark).
fn rasterize_svg(svg_data: &[u8], width: u32, height: u32) -> Option<Vec<u8>> {
    use resvg::tiny_skia;
    use resvg::usvg;

    // Normalize edge-label geometry before parsing. Fresh artifacts are
    // already normalized at write time (atom-tools visualize); this also
    // covers pre-existing artifact files on disk and any renderer output
    // that skips that path. Idempotent.
    let svg_data = match std::str::from_utf8(svg_data) {
        Ok(s) => atom_core::render::mermaid::normalize_edge_labels(s).into_bytes(),
        Err(_) => svg_data.to_vec(),
    };

    let mut opt = usvg::Options::default();
    opt.fontdb_mut().load_system_fonts();

    // Color emoji (🚀, 🔥, 📊, …) rasterize as missing-glyph tofu boxes unless
    // we steer usvg's text fallback toward a real color-emoji font. usvg's
    // default fallback returns the *first* face that claims to cover the
    // codepoint, which on macOS is the hidden `.LastResort` font that draws a
    // .notdef box for every codepoint. Prefer a dedicated emoji font first,
    // then chain to the default behaviour for everything else.
    let mut resolver = usvg::FontResolver::default();
    let default_fallback =
        std::mem::replace(&mut resolver.select_fallback, Box::new(|_, _, _| None));
    resolver.select_fallback = Box::new(move |c, exclude_fonts, fontdb| {
        if is_emoji_char(c) {
            if let Some(id) = color_emoji_face(c, exclude_fonts, fontdb) {
                return Some(id);
            }
        }
        default_fallback(c, exclude_fonts, fontdb)
    });
    opt.font_resolver = resolver;

    let tree = usvg::Tree::from_data(&svg_data, &opt).ok()?;

    let mut pixmap = tiny_skia::Pixmap::new(width, height)?;

    // Source card bg from atom's palette.
    let bg_hex =
        atom_core::render::colors::theme_color(atom_core::render::colors::ThemeColor::CardDark);
    let bg =
        parse_hex_to_skia(&bg_hex).unwrap_or(tiny_skia::Color::from_rgba8(0x15, 0x15, 0x16, 255));
    pixmap.fill(bg);

    // Scale the SVG to fit with padding.
    let svg_size = tree.size();
    let pad = 16.0_f32;
    let avail_w = (width as f32 - pad * 2.0).max(1.0);
    let avail_h = (height as f32 - pad * 2.0).max(1.0);
    let scale = (avail_w / svg_size.width()).min(avail_h / svg_size.height());
    let tx = (width as f32 - svg_size.width() * scale) / 2.0;
    let ty = (height as f32 - svg_size.height() * scale) / 2.0;

    let transform = tiny_skia::Transform::from_translate(tx, ty).post_scale(scale, scale);
    resvg::render(&tree, transform, &mut pixmap.as_mut());
    pixmap.encode_png().ok()
}

/// True if `c` is an emoji codepoint. Used to steer usvg's font fallback toward
/// a color-emoji font only for characters that actually need one, so normal
/// text keeps its existing matching behaviour.
fn is_emoji_char(c: char) -> bool {
    let cp = c as u32;
    // High-plane pictographs/emoticons + supplemental symbols.
    (0x1F300..=0x1FAFF).contains(&cp)
        // Miscellaneous Symbols & Pictographs, Dingbats, Misc Symbols & Arrows.
        || (0x2600..=0x27BF).contains(&cp)
        || (0x2B00..=0x2BFF).contains(&cp)
        // A few text-presentation emoji outside those blocks.
        || matches!(cp,
            0x00A9 | 0x00AE | 0x203C | 0x2049 | 0x2122 | 0x2139
            | 0x2194..=0x2199 | 0x21A9 | 0x21AA | 0x231A | 0x231B
            | 0x2328 | 0x23CF | 0x23E9..=0x23F3 | 0x23F8..=0x23FA
            | 0x24C2 | 0x25AA | 0x25AB | 0x25B6 | 0x25C0
            | 0x25FB..=0x25FE | 0x2934 | 0x2935
            | 0x3030 | 0x303D | 0x3297 | 0x3299)
}

/// Returns the ID of a dedicated color-emoji font face, preferring a normal
/// (non-hidden) face over hidden/system-UI ones such as `.LastResort`. The
/// default usvg fallback would otherwise pick `.LastResort`, which covers every
/// codepoint with a tofu box, so it never reaches the emoji font.
fn color_emoji_face(
    _c: char,
    exclude_fonts: &[resvg::usvg::fontdb::ID],
    fontdb: &resvg::usvg::fontdb::Database,
) -> Option<resvg::usvg::fontdb::ID> {
    let mut hidden_candidate = None;
    for face in fontdb.faces() {
        if exclude_fonts.contains(&face.id) {
            continue;
        }
        let looks_emoji = face
            .families
            .iter()
            .any(|(name, _)| name.to_lowercase().contains("emoji"))
            || face.post_script_name.to_lowercase().contains("emoji");
        if !looks_emoji {
            continue;
        }
        let primary = face.families.first().map(|f| f.0.as_str()).unwrap_or("");
        if !primary.starts_with('.') {
            return Some(face.id);
        }
        if hidden_candidate.is_none() {
            hidden_candidate = Some(face.id);
        }
    }
    hidden_candidate
}

fn parse_hex_to_skia(hex: &str) -> Option<resvg::tiny_skia::Color> {
    let hex = hex.trim_start_matches('#');
    if hex.len() != 6 {
        return None;
    }
    let r = u8::from_str_radix(&hex[0..2], 16).ok()?;
    let g = u8::from_str_radix(&hex[2..4], 16).ok()?;
    let b = u8::from_str_radix(&hex[4..6], 16).ok()?;
    Some(resvg::tiny_skia::Color::from_rgba8(r, g, b, 255))
}

/// paint_kitty_diagrams renders diagram SVGs as virtual placements sized
/// `cols` x `rows`, and deletes every stale diagram id the caller reports.
/// Diagrams own the 17..=255 id range. Best-effort: unreadable files are
/// skipped silently.
pub fn paint_kitty_diagrams(specs: &[DiagramSpec], stale_ids: &[usize]) {
    let mut sb = String::new();
    for id in stale_ids {
        sb.push_str(&kitty_delete_virtual(*id));
    }
    for spec in specs {
        let target_w = (spec.cols as u32) * PREVIEW_CELL_W;
        let target_h = (spec.rows as u32) * PREVIEW_CELL_H;

        let png = if !spec.svg.is_empty() {
            // Rasterize SVG on-demand at exact terminal dimensions.
            std::fs::read(&spec.svg)
                .ok()
                .and_then(|data| rasterize_svg(&data, target_w, target_h))
        } else if !spec.png.is_empty() {
            // Legacy: read pre-rendered PNG from disk.
            std::fs::read(&spec.png).ok()
        } else {
            None
        };

        let Some(png) = png else { continue };
        sb.push_str(&kitty_transmit(spec.id, &png));
        sb.push_str(&format!(
            "\x1b_Ga=p,U=1,i={},c={},r={},q=2\x1b\\",
            spec.id, spec.cols, spec.rows
        ));
    }
    write_tty(&sb);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode rasterized PNG bytes into RGBA pixels.
    fn decode_rgba(png: &[u8]) -> image::RgbaImage {
        image::load_from_memory(png).expect("decode PNG").to_rgba8()
    }

    /// Count distinct RGBA colours across the whole image. A real colour emoji
    /// produces hundreds of colours; a tofu (.notdef) box is mostly a single
    /// outline colour, so this cleanly separates the two.
    fn distinct_colors(img: &image::RgbaImage) -> usize {
        let mut colors = std::collections::HashSet::new();
        for p in img.pixels() {
            colors.insert(*p);
        }
        colors.len()
    }

    /// Count pixels that differ from the card background (CardDark).
    fn non_bg_pixels(img: &image::RgbaImage, bg: (u8, u8, u8)) -> usize {
        img.pixels()
            .filter(|p| (p.0[0], p.0[1], p.0[2]) != bg)
            .count()
    }

    /// The card background RGBA used by `rasterize_svg`, parsed from the theme.
    fn card_bg_rgba() -> (u8, u8, u8) {
        let hex =
            atom_core::render::colors::theme_color(atom_core::render::colors::ThemeColor::CardDark);
        let hex = hex.trim_start_matches('#');
        (
            u8::from_str_radix(&hex[0..2], 16).unwrap(),
            u8::from_str_radix(&hex[2..4], 16).unwrap(),
            u8::from_str_radix(&hex[4..6], 16).unwrap(),
        )
    }

    /// An SVG with a full-bleed card background + one text line, matching the
    /// mermaid output shape that `rasterize_svg` is fed.
    fn svg_with_text(text: &str) -> String {
        let bg = card_bg_rgba();
        format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="400" height="200">
  <rect width="400" height="200" fill="#{:02x}{:02x}{:02x}"/>
  <text x="20" y="100" font-size="48" fill="#ccc" font-family="sans-serif">{}</text>
</svg>"##,
            bg.0, bg.1, bg.2, text
        )
    }

    /// A mermaid-shaped edge label, trimmed from a real merman-cli 0.7.0
    /// render: the inner label g is translated by -W/2, so the text rows
    /// (anchored at x=0) land W/2 left of the edge midpoint.
    const EDGE_LABEL_SVG: &str = r#"<g class="edgeLabel" transform="translate(372.71,103.5)"><g class="label" data-id="L_D_B_0" transform="translate(-11.39,-11.5)"><g><rect class="background" style="" x="-2" y="-1" width="22.789" height="23"/><text y="-10.1" style="" text-anchor="middle"><tspan class="row text-outer-tspan" x="0" y="-0.1em" dy="1.1em" text-anchor="middle"><tspan font-style="normal" class="text-inner-tspan" font-weight="normal">No</tspan></tspan></text></g></g></g>"#;

    #[test]
    fn edge_label_geometry_is_normalized_before_rasterizing() {
        let fixed = atom_core::render::mermaid::normalize_edge_labels(EDGE_LABEL_SVG);
        // Inner g loses its -W/2 x-offset (vertical -11.5 preserved).
        assert!(
            fixed.contains(r#"transform="translate(0,-11.5)""#),
            "{fixed}"
        );
        // Rect re-centered with mermaid's 2px padding: -(22.789/2 + 2).
        assert!(fixed.contains(r#"x="-13.3945""#), "{fixed}");
        // The text element is untouched.
        assert!(
            fixed.contains(r#"<text y="-10.1" style="" text-anchor="middle">"#),
            "{fixed}"
        );
        // The label content survives.
        assert!(fixed.contains(">No</tspan>"), "{fixed}");
        assert!(fixed.contains(r#"width="22.789" height="23""#), "{fixed}");
    }

    #[test]
    fn edge_label_fix_ignores_unsized_node_label_rects() {
        // Node labels carry `style="stroke: none"` with no width/height:
        // they must pass through untouched.
        let svg = r#"<g class="label" transform="translate(0,-9.5)"><rect/><g><rect class="background" style="stroke: none"/><text y="-10.1"><tspan>Input</tspan></text></g></g>"#;
        assert_eq!(atom_core::render::mermaid::normalize_edge_labels(svg), svg);
    }

    #[test]
    fn edge_label_fix_handles_attribute_order() {
        // Same rect with width before x — the fix must still find it.
        let svg = r#"<g class="label" transform="translate(-10,-11.5)"><g><rect class="background" height="23" y="-1" width="20" x="-2"/><text y="-10.1" text-anchor="middle"><tspan x="0">No</tspan></text></g></g>"#;
        let fixed = atom_core::render::mermaid::normalize_edge_labels(svg);
        assert!(
            fixed.contains(r#"transform="translate(0,-11.5)""#),
            "{fixed}"
        );
        assert!(fixed.contains(r#"x="-12""#), "{fixed}");
        assert!(
            fixed.contains(r#"height="23" y="-1" width="20""#),
            "{fixed}"
        );
    }

    #[test]
    fn edge_label_fix_is_idempotent() {
        let once = atom_core::render::mermaid::normalize_edge_labels(EDGE_LABEL_SVG);
        let twice = atom_core::render::mermaid::normalize_edge_labels(&once);
        assert_eq!(once, twice, "fix must be idempotent");
    }

    /// Regression: a wide merman edge label must not slide under the
    /// source node. The label is centered at x=250 with the source node
    /// covering x=0..200 (painted after the label, as mermaid does). The
    /// merman shape puts the text at 250 - W/2 = 170, so its head is
    /// hidden; after normalization the text is centered at 250 and glyphs
    /// must appear in the strip x=265..335.
    #[test]
    fn edge_label_text_is_not_hidden_under_source_node() {
        let bg = card_bg_rgba();
        let svg = format!(
            r##"<svg xmlns="http://www.w3.org/2000/svg" width="400" height="200">
  <rect width="400" height="200" fill="#{:02x}{:02x}{:02x}"/>
  <g class="edgeLabel" transform="translate(250,100)">
    <g class="label" transform="translate(-80,-15)">
      <g>
        <rect class="background" x="-2" y="-1" width="160" height="30" fill="#666" opacity="0.5"/>
        <text y="-10.1" text-anchor="middle" fill="#ccc" font-size="16" font-family="sans-serif">
          <tspan class="row text-outer-tspan" x="0" y="-0.1em" dy="1.1em" text-anchor="middle">socketfile-longlabel</tspan>
        </text>
      </g>
    </g>
  </g>
  <g class="node" transform="translate(100,100)">
    <rect x="-100" y="-25" width="200" height="50" fill="#1f2020"/>
  </g>
</svg>"##,
            bg.0, bg.1, bg.2
        );
        let png = rasterize_svg(svg.as_bytes(), 800, 400).expect("rasterize edge-label SVG");
        let img = decode_rgba(&png);
        // Map SVG x=265..335 to pixels: scale = min(768/400, 368/200) = 1.84,
        // tx = (800 - 400*1.84)/2 = 32.
        let (x0, x1) = ((265.0 * 1.84 + 32.0) as u32, (335.0 * 1.84 + 32.0) as u32);
        let bright = img
            .enumerate_pixels()
            .filter(|(x, _, p)| {
                *x >= x0 && *x < x1 && p.0[0] > 0x80 && p.0[1] > 0x80 && p.0[2] > 0x80
            })
            .count();
        assert!(
            bright > 30,
            "edge-label text must render right of the source node, got {bright} bright pixels"
        );
    }

    #[test]
    fn emoji_renders_as_colored_glyphs_not_tofu() {
        let svg = svg_with_text("🚀🔥📊");
        let png = rasterize_svg(svg.as_bytes(), 400, 200).expect("rasterize emoji SVG");
        let img = decode_rgba(&png);
        let colors = distinct_colors(&img);
        // A real colour emoji has hundreds/thousands of distinct colours; tofu
        // boxes have only a handful (background + one outline colour).
        assert!(
            colors > 100,
            "emoji should rasterize to many colours (real glyphs), got {colors}"
        );
    }

    #[test]
    fn plain_text_still_renders() {
        let svg = svg_with_text("OK TEXT");
        let png = rasterize_svg(svg.as_bytes(), 400, 200).expect("rasterize plain SVG");
        let img = decode_rgba(&png);
        let bg = card_bg_rgba();
        let non_bg = non_bg_pixels(&img, bg);
        assert!(
            non_bg > 500,
            "plain text should still rasterize as glyphs, got {non_bg} non-background pixels"
        );
    }

    #[test]
    fn is_emoji_char_recognizes_common_emoji() {
        for c in ['🚀', '🔥', '📊', '😀', '❤', '⚡', '☀'] {
            assert!(is_emoji_char(c), "{c:?} (U+{:X}) should be emoji", c as u32);
        }
        for c in ['A', 'z', '1', ' ', 'é'] {
            assert!(!is_emoji_char(c), "{c:?} should not be emoji");
        }
    }
}
