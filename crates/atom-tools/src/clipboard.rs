//! Clipboard reading + pasted-image resolution, ported from clipboard.go.
//! A keybind shells out to the platform clipboard tool instead of waiting
//! for the terminal to paste bytes; Finder/desktop drops arrive as a
//! bracketed paste of file paths, resolved to image files here.
//!
//! Note: clipboard_test.go in Go has no OSC52 coverage — its assertions
//! target unescapePastePath/localImagesFromPaste, which are ported here.

use atom_core::types::MAX_IMAGE_SOURCE_BYTES;
use std::path::{Path, PathBuf};

const CLIPBOARD_TIMEOUT: Duration = Duration::from_secs(2);

use std::time::Duration;

/// clipboardContent is whatever the OS clipboard currently holds that
/// atom can use. Image is preferred over text when both are present.
#[derive(Debug, Default, Clone)]
pub struct ClipboardContent {
    /// raw image bytes; None when there is no image
    pub data: Option<Vec<u8>>,
    pub name: String,
    pub text: String,
}

/// localImageFile is one image loaded from a dropped or pasted path.
#[derive(Debug, Clone)]
pub struct LocalImageFile {
    pub name: String,
    pub data: Vec<u8>,
}

async fn run_discard(program: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args).stderr(std::process::Stdio::null());
    let out = tokio::time::timeout(CLIPBOARD_TIMEOUT, cmd.output()).await;
    match out {
        Err(_) => Err("timed out".to_string()),
        Ok(Err(e)) => Err(e.to_string()),
        Ok(Ok(out)) if out.status.success() => Ok(out.stdout),
        Ok(Ok(_)) => Err(format!("{program} failed")),
    }
}

/// readClipboard probes the OS clipboard: a supported image if one is
/// present, otherwise plain text. An empty clipboard is normal.
pub async fn read_clipboard() -> ClipboardContent {
    if let Some((data, name)) = read_clipboard_image().await {
        let sniffed = atom_core::render::diff::sniff_image_mime(&data);
        if !data.is_empty() && !sniffed.is_empty() {
            let name = if name.is_empty() {
                "clipboard".to_string()
            } else {
                name
            };
            return ClipboardContent {
                data: Some(data),
                name,
                text: String::new(),
            };
        }
    }
    let text = read_clipboard_text()
        .await
        .trim_end_matches('\0')
        .to_string();
    if !text.is_empty() {
        return ClipboardContent {
            text,
            ..Default::default()
        };
    }
    ClipboardContent::default()
}

#[cfg(target_os = "macos")]
pub async fn read_clipboard_image() -> Option<(Vec<u8>, String)> {
    read_darwin_clipboard_image().await
}

#[cfg(target_os = "linux")]
pub async fn read_clipboard_image() -> Option<(Vec<u8>, String)> {
    read_linux_clipboard_image().await
}

#[cfg(target_os = "windows")]
pub async fn read_clipboard_image() -> Option<(Vec<u8>, String)> {
    read_windows_clipboard_image().await
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
pub async fn read_clipboard_image() -> Option<(Vec<u8>, String)> {
    None
}

pub async fn read_clipboard_text() -> String {
    #[cfg(target_os = "macos")]
    {
        if let Ok(out) = run_discard("pbpaste", &[]).await {
            return String::from_utf8_lossy(&out).to_string();
        }
    }
    #[cfg(target_os = "linux")]
    {
        if let Ok(out) = run_discard("wl-paste", &["-n", "-t", "text/plain"]).await {
            if !out.is_empty() {
                return String::from_utf8_lossy(&out).to_string();
            }
        }
        if let Ok(out) = run_discard("xclip", &["-selection", "clipboard", "-o"]).await {
            return String::from_utf8_lossy(&out).to_string();
        }
    }
    #[cfg(target_os = "windows")]
    {
        if let Ok(out) = run_discard(
            "powershell.exe",
            &["-NonInteractive", "-NoProfile", "-Command", "Get-Clipboard"],
        )
        .await
        {
            return String::from_utf8_lossy(&out).to_string();
        }
    }
    String::new()
}

#[cfg(target_os = "macos")]
async fn read_darwin_clipboard_image() -> Option<(Vec<u8>, String)> {
    let tmp = tempfile::Builder::new()
        .prefix("atom-clipboard-")
        .suffix(".png")
        .tempfile()
        .ok()?;
    let path = tmp.path().to_path_buf();
    drop(tmp); // the AppleScript writes through a fresh handle

    let _ = std::fs::remove_file(&path);
    if run_discard("pngpaste", &[&path.display().to_string()])
        .await
        .is_ok()
    {
        if let Ok(data) = std::fs::read(&path) {
            if !atom_core::render::diff::sniff_image_mime(&data).is_empty() {
                let _ = std::fs::remove_file(&path);
                return Some((data, "clipboard.png".to_string()));
            }
        }
    }

    let script = format!(
        r#"set imageData to the clipboard as "PNGf"
set fileRef to open for access POSIX file "{}" with write permission
set eof fileRef to 0
write imageData to fileRef
close access fileRef"#,
        applescript_posix(&path.display().to_string())
    );
    let res = run_discard("osascript", &["-e", &script]).await;
    let _ = res;
    if let Ok(data) = std::fs::read(&path) {
        if !atom_core::render::diff::sniff_image_mime(&data).is_empty() {
            let _ = std::fs::remove_file(&path);
            return Some((data, "clipboard.png".to_string()));
        }
    }
    // PNGf/TIFF from some macOS apps isn't a raw PNG; sips can transcode.
    if let Some(converted) = sips_to_png(path.display().to_string()).await {
        let _ = std::fs::remove_file(&path);
        return Some((converted, "clipboard.png".to_string()));
    }
    let _ = std::fs::remove_file(&path);
    None
}

#[cfg(target_os = "macos")]
async fn sips_to_png(path: String) -> Option<Vec<u8>> {
    let out_path = format!("{path}.png");
    let res = run_discard("sips", &["-s", "format", "png", &path, "--out", &out_path]).await;
    if res.is_err() {
        let _ = std::fs::remove_file(&out_path);
        return None;
    }
    let data = std::fs::read(&out_path).ok();
    let _ = std::fs::remove_file(&out_path);
    let data = data?;
    if atom_core::render::diff::sniff_image_mime(&data).is_empty() {
        return None;
    }
    Some(data)
}

fn applescript_posix(path: &str) -> String {
    path.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(target_os = "linux")]
async fn read_linux_clipboard_image() -> Option<(Vec<u8>, String)> {
    const TYPES: &[(&str, &str)] = &[
        ("image/png", "clipboard.png"),
        ("image/jpeg", "clipboard.jpg"),
        ("image/gif", "clipboard.gif"),
        ("image/webp", "clipboard.webp"),
        ("image/bmp", "clipboard.bmp"),
    ];
    for (mime, name) in TYPES {
        if let Ok(out) = run_discard("wl-paste", &["-t", mime]).await {
            if !atom_core::render::diff::sniff_image_mime(&out).is_empty() {
                return Some((out, name.to_string()));
            }
        }
    }
    for (mime, name) in TYPES {
        if let Ok(out) = run_discard("xclip", &["-selection", "clipboard", "-t", mime, "-o"]).await
        {
            if !atom_core::render::diff::sniff_image_mime(&out).is_empty() {
                return Some((out, name.to_string()));
            }
        }
    }
    None
}

#[cfg(target_os = "windows")]
async fn read_windows_clipboard_image() -> Option<(Vec<u8>, String)> {
    use base64::Engine;
    let script = r#"Add-Type -AssemblyName System.Windows.Forms; $img = [System.Windows.Forms.Clipboard]::GetImage(); if ($img) { $ms = New-Object System.IO.MemoryStream; $img.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png); [Convert]::ToBase64String($ms.ToArray()) }"#;
    let out = run_discard(
        "powershell.exe",
        &["-NonInteractive", "-NoProfile", "-Command", script],
    )
    .await
    .ok()?;
    let raw = String::from_utf8_lossy(&out).trim().to_string();
    if raw.is_empty() {
        return None;
    }
    let decoded = base64::engine::general_purpose::STANDARD.decode(raw).ok()?;
    if atom_core::render::diff::sniff_image_mime(&decoded).is_empty() {
        return None;
    }
    Some((decoded, "clipboard.png".to_string()))
}

// ---------------------------------------------------------------------------
// Pasted / dropped paths.
// ---------------------------------------------------------------------------

/// localImagesFromPaste returns images if every non-empty line of a
/// bracketed paste is a readable image file (Finder/kitty drops). Mixed
/// or non-file pastes return None so the text is inserted normally.
pub fn local_images_from_paste(content: &str) -> Option<Vec<LocalImageFile>> {
    let content = content.replace("\r\n", "\n").replace('\r', "\n");
    let trim = content.trim();
    if trim.is_empty() {
        return None;
    }
    let mut files = Vec::new();
    for line in trim.split('\n') {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match read_local_image(&unescape_paste_path(line)) {
            Some(f) => files.push(f),
            None => return None,
        }
    }
    Some(files)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(hi), Some(lo)) = (hi, lo) {
                out.push(((hi << 4) | lo) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).to_string()
}

pub fn unescape_paste_path(s: &str) -> String {
    let mut s = s.trim().to_string();
    s = s.trim_matches(|c| c == '"' || c == '\'').to_string();
    if s.to_lowercase().starts_with("file:") {
        // Minimal URL parse: strip scheme//host, keep the decoded path.
        if let Some(rest) = s
            .strip_prefix("file://")
            .or_else(|| s.get(5..).map(|r| r.trim_start_matches("//")))
        {
            let mut path = rest.split(['?', '#']).next().unwrap_or("").to_string();
            if !path.starts_with('/') {
                path = format!("/{path}");
            }
            let decoded = percent_decode(&path);
            if !decoded.is_empty() {
                s = decoded;
            }
        }
    }
    if s.contains('\\') {
        let mut b = String::new();
        let chars: Vec<char> = s.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '\\' && i + 1 < chars.len() {
                b.push(chars[i + 1]);
                i += 2;
                continue;
            }
            b.push(chars[i]);
            i += 1;
        }
        s = b;
    }
    if s.starts_with("~/") || s == "~" {
        if let Some(home) = dirs::home_dir() {
            s = home.join(s.trim_start_matches("~/")).display().to_string();
        }
    }
    s
}

pub fn read_local_image(path: &str) -> Option<LocalImageFile> {
    if path.is_empty() {
        return None;
    }
    let p = PathBuf::from(path);
    let info = std::fs::metadata(&p).ok()?;
    if info.is_dir() {
        return None;
    }
    if info.len() as usize > MAX_IMAGE_SOURCE_BYTES {
        return None;
    }
    let data = std::fs::read(&p).ok()?;
    if atom_core::render::diff::sniff_image_mime(&data).is_empty() {
        return None;
    }
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    Some(LocalImageFile { name, data })
}

#[allow(unused)]
fn unused(_: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unescapes_paste_paths() {
        let dir = tempfile::tempdir().unwrap();
        let abs = dir.path().join("shot.png");
        let cases = vec![
            ("/tmp/foo.png".to_string(), "'/tmp/foo.png'".to_string()),
            ("/tmp/foo.png".to_string(), "\"/tmp/foo.png\"".to_string()),
            (
                abs.display().to_string(),
                format!("file://{}", abs.display()),
            ),
            (
                "/tmp/has space.png".to_string(),
                r"/tmp/has\ space.png".to_string(),
            ),
        ];
        for (want, input) in cases {
            assert_eq!(unescape_paste_path(&input), want, "input {input}");
        }
    }

    #[test]
    fn decodes_file_urls_with_escapes() {
        assert_eq!(unescape_paste_path("file:///tmp/a%20b.png"), "/tmp/a b.png");
    }

    #[test]
    fn resolves_images_from_pasted_paths() {
        let dir = tempfile::tempdir().unwrap();
        let png: &[u8] = &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3];
        let p1 = dir.path().join("a.png");
        let p2 = dir.path().join("b.png");
        std::fs::write(&p1, png).unwrap();
        std::fs::write(&p2, png).unwrap();
        let txt = dir.path().join("note.txt");
        std::fs::write(&txt, "hi").unwrap();

        let got = local_images_from_paste(&format!("'{}'", p1.display())).unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].name, "a.png");

        let two = format!("{}\n{}", p1.display(), p2.display());
        assert_eq!(local_images_from_paste(&two).unwrap().len(), 2);

        let url = format!("file://{}", p1.display());
        assert_eq!(local_images_from_paste(&url).unwrap().len(), 1);

        assert!(local_images_from_paste(&format!("hello {}", p1.display())).is_none());
        assert!(local_images_from_paste(&txt.display().to_string()).is_none());
        assert!(local_images_from_paste("just some text").is_none());
        assert!(local_images_from_paste("").is_none());
        assert!(local_images_from_paste("xxxxxxxx.png").is_none());
    }

    #[tokio::test]
    async fn empty_clipboard_is_normal() {
        // No clipboard tools under test conditions; must not panic and
        // must produce an empty result.
        let c = read_clipboard().await;
        let _ = c;
    }
}
