// clipboard.go reads images (and text) from the OS clipboard the way
// OpenCode does: a keybind (Ctrl+V / Super+V) shells out to the platform
// clipboard tool instead of waiting for the terminal to paste bytes.
// Finder/desktop drops still arrive as bracketed paste of file paths;
// those are resolved to image files here too.
package main

import (
	"bytes"
	"context"
	"encoding/base64"
	"io"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"time"
)

const clipboardTimeout = 2 * time.Second

// clipboardContent is whatever the OS clipboard currently holds that
// atom can use. Image is preferred over text when both are present.
type clipboardContent struct {
	data []byte // raw image bytes; nil when there is no image
	name string
	text string
}

// localImageFile is one image loaded from a dropped or pasted path.
type localImageFile struct {
	name string
	data []byte
}

// readClipboard probes the OS clipboard: a supported image if one is
// present, otherwise plain text. An empty clipboard is normal.
func readClipboard() clipboardContent {
	if data, name := readClipboardImage(); len(data) > 0 && sniffImageMIME(data) != "" {
		if name == "" {
			name = "clipboard"
		}
		return clipboardContent{data: data, name: name}
	}
	if text := strings.TrimRight(readClipboardText(), "\x00"); text != "" {
		return clipboardContent{text: text}
	}
	return clipboardContent{}
}

func readClipboardImage() ([]byte, string) {
	switch runtime.GOOS {
	case "darwin":
		return readDarwinClipboardImage()
	case "linux":
		return readLinuxClipboardImage()
	case "windows":
		return readWindowsClipboardImage()
	}
	return nil, ""
}

func readClipboardText() string {
	switch runtime.GOOS {
	case "darwin":
		out, err := runDiscardErr("pbpaste")
		if err == nil {
			return string(out)
		}
	case "linux":
		if out, err := runDiscardErr("wl-paste", "-n", "-t", "text/plain"); err == nil && len(out) > 0 {
			return string(out)
		}
		if out, err := runDiscardErr("xclip", "-selection", "clipboard", "-o"); err == nil {
			return string(out)
		}
	case "windows":
		out, err := runDiscardErr("powershell.exe", "-NonInteractive", "-NoProfile", "-Command", "Get-Clipboard")
		if err == nil {
			return string(out)
		}
	}
	return ""
}

func readDarwinClipboardImage() ([]byte, string) {
	tmp, err := os.CreateTemp("", "atom-clipboard-*.png")
	if err != nil {
		return nil, ""
	}
	path := tmp.Name()
	tmp.Close()
	defer os.Remove(path)

	if _, err := runDiscardErr("pngpaste", path); err == nil {
		if data, err := os.ReadFile(path); err == nil && sniffImageMIME(data) != "" {
			return data, "clipboard.png"
		}
	}

	script := `set imageData to the clipboard as "PNGf"
set fileRef to open for access POSIX file "` + applescriptPOSIX(path) + `" with write permission
set eof fileRef to 0
write imageData to fileRef
close access fileRef`
	if _, err := runDiscardErr("osascript", "-e", script); err != nil {
		return nil, ""
	}
	if data, err := os.ReadFile(path); err == nil && sniffImageMIME(data) != "" {
		return data, "clipboard.png"
	}
	// PNGf/TIFF from some macOS apps isn't a raw PNG; sips can transcode.
	if converted := sipsToPNG(path); len(converted) > 0 {
		return converted, "clipboard.png"
	}
	return nil, ""
}

func sipsToPNG(path string) []byte {
	out := path + ".png"
	defer os.Remove(out)
	if _, err := runDiscardErr("sips", "-s", "format", "png", path, "--out", out); err != nil {
		return nil
	}
	data, err := os.ReadFile(out)
	if err != nil || sniffImageMIME(data) == "" {
		return nil
	}
	return data
}

func applescriptPOSIX(path string) string {
	path = strings.ReplaceAll(path, "\\", "\\\\")
	return strings.ReplaceAll(path, `"`, `\"`)
}

func readLinuxClipboardImage() ([]byte, string) {
	types := []struct {
		mime string
		name string
	}{
		{"image/png", "clipboard.png"},
		{"image/jpeg", "clipboard.jpg"},
		{"image/gif", "clipboard.gif"},
		{"image/webp", "clipboard.webp"},
		{"image/bmp", "clipboard.bmp"},
	}
	for _, t := range types {
		if out, err := runDiscardErr("wl-paste", "-t", t.mime); err == nil && sniffImageMIME(out) != "" {
			return out, t.name
		}
	}
	for _, t := range types {
		if out, err := runDiscardErr("xclip", "-selection", "clipboard", "-t", t.mime, "-o"); err == nil && sniffImageMIME(out) != "" {
			return out, t.name
		}
	}
	return nil, ""
}

func readWindowsClipboardImage() ([]byte, string) {
	script := `Add-Type -AssemblyName System.Windows.Forms; $img = [System.Windows.Forms.Clipboard]::GetImage(); if ($img) { $ms = New-Object System.IO.MemoryStream; $img.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png); [Convert]::ToBase64String($ms.ToArray()) }`
	out, err := runDiscardErr("powershell.exe", "-NonInteractive", "-NoProfile", "-Command", script)
	if err != nil {
		return nil, ""
	}
	raw := bytes.TrimSpace(out)
	if len(raw) == 0 {
		return nil, ""
	}
	decoded := make([]byte, base64.StdEncoding.DecodedLen(len(raw)))
	n, err := base64.StdEncoding.Decode(decoded, raw)
	if err != nil || sniffImageMIME(decoded[:n]) == "" {
		return nil, ""
	}
	return decoded[:n], "clipboard.png"
}

func runDiscardErr(name string, args ...string) ([]byte, error) {
	ctx, cancel := context.WithTimeout(context.Background(), clipboardTimeout)
	defer cancel()
	cmd := exec.CommandContext(ctx, name, args...)
	cmd.Stderr = io.Discard
	return cmd.Output()
}

// localImagesFromPaste returns images if every non-empty line of a
// bracketed paste is a readable image file (Finder/kitty drops). Mixed
// or non-file pastes return nil so the text is inserted normally.
func localImagesFromPaste(content string) []localImageFile {
	content = strings.ReplaceAll(content, "\r\n", "\n")
	content = strings.ReplaceAll(content, "\r", "\n")
	trim := strings.TrimSpace(content)
	if trim == "" {
		return nil
	}
	var files []localImageFile
	for _, line := range strings.Split(trim, "\n") {
		line = strings.TrimSpace(line)
		if line == "" {
			continue
		}
		f, ok := readLocalImage(unescapePastePath(line))
		if !ok {
			return nil
		}
		files = append(files, f)
	}
	return files
}

func unescapePastePath(s string) string {
	s = strings.TrimSpace(s)
	s = strings.Trim(s, `"'`)
	if strings.HasPrefix(strings.ToLower(s), "file:") {
		if u, err := url.Parse(s); err == nil && u.Path != "" {
			s = u.Path
			if runtime.GOOS == "windows" && strings.HasPrefix(s, "/") && len(s) > 2 && s[2] == ':' {
				s = s[1:]
			}
		}
	}
	if strings.Contains(s, `\`) {
		var b strings.Builder
		for i := 0; i < len(s); i++ {
			if s[i] == '\\' && i+1 < len(s) {
				b.WriteByte(s[i+1])
				i++
				continue
			}
			b.WriteByte(s[i])
		}
		s = b.String()
	}
	if strings.HasPrefix(s, "~/") || s == "~" {
		if home, err := os.UserHomeDir(); err == nil {
			s = filepath.Join(home, strings.TrimPrefix(s, "~/"))
		}
	}
	return s
}

func readLocalImage(path string) (localImageFile, bool) {
	if path == "" {
		return localImageFile{}, false
	}
	info, err := os.Stat(path)
	if err != nil || info.IsDir() {
		return localImageFile{}, false
	}
	if info.Size() > maxImageSourceBytes {
		return localImageFile{}, false
	}
	data, err := os.ReadFile(path)
	if err != nil || sniffImageMIME(data) == "" {
		return localImageFile{}, false
	}
	return localImageFile{name: filepath.Base(path), data: data}, true
}
