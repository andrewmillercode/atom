// preview.go renders the pasted-image previews shown above the prompt
// input, and parses the terminal paste sequences that deliver images.
//
// Terminals that speak the Kitty graphics protocol (kitty, WezTerm,
// Ghostty, foot, Konsole, ...) get a real thumbnail: the preview is a
// small PNG, with the marker number drawn into its bottom-left corner.
// Graphics commands go out-of-band to /dev/tty only to transmit or
// delete a virtual placement (U=1). The visible preview is Unicode
// placeholders (U+10EEEE) in the View string, in the reserved rows
// above the prompt — the same fixed bottom chrome as the footer.
// Bubble Tea moves those cells with the frame, so scrolling does not
// retransmit or CUP-place images. Other terminals get no preview rows;
// the IMG chip in the prompt is the only indicator.
package main

import (
	"bytes"
	"encoding/base64"
	"fmt"
	"image"
	"image/color"
	_ "image/gif" // registers the GIF decoder for image.Decode
	"image/jpeg"
	"image/png"
	"math"
	"os"
	"strconv"
	"strings"

	"github.com/charmbracelet/x/ansi"
)

// kittyMaxChunk is the Kitty protocol's maximum payload per transmission
// chunk, before base64 encoding. Larger images must be chunked with m=1
// on every chunk except the last.
const kittyMaxChunk = 4096

// writeTTY writes bytes straight to the terminal. The Bubble Tea cell
// renderer drops unknown escape sequences, so out-of-band protocol
// traffic (kitty transmit/delete) goes via /dev/tty. It is a
// variable so tests can capture the writes.
var writeTTY = func(s string) {
	if !isTerminal(os.Stdout) {
		return
	}
	tty, err := os.OpenFile("/dev/tty", os.O_WRONLY, 0)
	if err != nil {
		return
	}
	defer tty.Close()
	_, _ = tty.WriteString(s)
}

// --- paste parsing ---

// parseOSC1337 extracts an image from an iTerm2/WezTerm image paste:
// "ESC ] 1337 ; File=name=...;inline=1;<base64> BEL". The base64 payload
// is everything after the last ';' (older terminals add a "base64,"
// prefix there). ok is false when the sequence isn't an inline image.
func parseOSC1337(seq string) (name string, data []byte, ok bool) {
	seq = strings.TrimPrefix(seq, "\x1b]")
	seq = strings.TrimSuffix(seq, "\a")
	seq = strings.TrimSuffix(seq, "\x1b\\")
	rest, found := strings.CutPrefix(seq, "1337;")
	if !found {
		return "", nil, false
	}
	// Accept File= and MultipartFile= (the same shape).
	body, found := strings.CutPrefix(rest, "File=")
	if !found {
		if body, found = strings.CutPrefix(rest, "MultipartFile="); !found {
			return "", nil, false
		}
	}
	// The args are ';'-separated; the payload is the last segment.
	last := strings.LastIndex(body, ";")
	if last < 0 {
		return "", nil, false
	}
	args := body[:last]
	payload := strings.TrimPrefix(body[last+1:], "base64,")
	// Only inline images are pastes; anything else is a download.
	if !strings.Contains(args, "inline=1") {
		return "", nil, false
	}
	for _, arg := range strings.Split(args, ";") {
		k, v, found := strings.Cut(arg, "=")
		if !found || k != "name" {
			continue
		}
		// iTerm2 base64-encodes names with special characters. Accept
		// the decoded form when it's valid UTF-8 text, else use the raw.
		if dec, err := base64.StdEncoding.DecodeString(v); err == nil && len(dec) > 0 && bytes.IndexByte(dec, 0) < 0 {
			name = string(dec)
		} else {
			name = v
		}
	}
	data, err := base64.StdEncoding.DecodeString(payload)
	if err != nil {
		return "", nil, false
	}
	return name, data, true
}

// pasteSegment is one piece of a pasted string: either plain text or an
// image payload.
type pasteSegment struct {
	text string
	data []byte
}

// splitPaste splits pasted content into text and image segments, so a
// paste mixing text and pictures inserts both in order.
func splitPasteSegments(content string) []pasteSegment {
	var segments []pasteSegment
	for {
		idx := strings.Index(content, "\x1b]1337;")
		if idx < 0 {
			if content != "" {
				segments = append(segments, pasteSegment{text: content})
			}
			return segments
		}
		if idx > 0 {
			segments = append(segments, pasteSegment{text: content[:idx]})
		}
		content = content[idx:]
		// The sequence ends at the first BEL or ESC \ (ST).
		end := strings.IndexAny(content, "\a\x1b")
		if end < 0 {
			segments = append(segments, pasteSegment{text: content})
			return segments
		}
		seq := content[:end+1]
		if content[end] == '\x1b' && end+1 < len(content) && content[end+1] == '\\' {
			seq = content[:end+2]
			content = content[end+2:]
		} else {
			content = content[end+1:]
		}
		if _, data, ok := parseOSC1337(seq); ok {
			segments = append(segments, pasteSegment{data: data})
		} else {
			// Not an inline image: keep the raw sequence as text so it
			// isn't silently dropped.
			segments = append(segments, pasteSegment{text: seq})
		}
	}
}

// kittyPasteData turns one Kitty graphics event's payload into image
// bytes. PNG payloads pass through; raw RGBA payloads (f=32) are
// re-encoded as PNG using the transmitted width and height. A missing
// format (0) passes the payload through — the caller's magic-byte sniff
// rejects anything that isn't a real image.
func kittyPasteData(payload []byte, format, w, h int) ([]byte, bool) {
	switch format {
	case 0, 100: // PNG (or untyped)
		return payload, true
	case 32: // RGBA
		if w <= 0 || h <= 0 || len(payload) != w*h*4 {
			return nil, false
		}
		img := image.NewRGBA(image.Rect(0, 0, w, h))
		copy(img.Pix, payload)
		var buf bytes.Buffer
		if err := png.Encode(&buf, img); err != nil {
			return nil, false
		}
		return buf.Bytes(), true
	case 24: // RGB
		if w <= 0 || h <= 0 || len(payload) != w*h*3 {
			return nil, false
		}
		img := image.NewRGBA(image.Rect(0, 0, w, h))
		for i := 0; i < w*h; i++ {
			img.Pix[i*4] = payload[i*3]
			img.Pix[i*4+1] = payload[i*3+1]
			img.Pix[i*4+2] = payload[i*3+2]
			img.Pix[i*4+3] = 0xFF
		}
		var buf bytes.Buffer
		if err := png.Encode(&buf, img); err != nil {
			return nil, false
		}
		return buf.Bytes(), true
	}
	return nil, false
}

// imageSize measures an image without decoding it fully. It errors when
// the format isn't decodable; the preview then falls back to a text row.
func imageSize(data []byte) (w, h int, err error) {
	cfg, _, err := image.DecodeConfig(bytes.NewReader(data))
	if err != nil {
		return 0, 0, err
	}
	return cfg.Width, cfg.Height, nil
}

// --- preview layout and rendering ---

// previewBox returns the preview's cell box (columns x rows, assuming
// 8x16 px cells) for an image of the given pixel size. Thumbnails sit
// in one row; they're small — at most 8 columns and 3 rows.
func previewBox(w, h int) (cols, rows int) {
	if w <= 0 || h <= 0 {
		return 6, 3
	}
	rows = 3
	cols = int(math.Round(float64(rows) * 2 * float64(w) / float64(h)))
	if cols > 8 {
		cols = 8
		rows = int(math.Round(float64(cols) * float64(h) / (2 * float64(w))))
	}
	if rows > 3 {
		rows = 3
	}
	if rows < 2 {
		rows = 2
	}
	if cols < 3 {
		cols = 3
	}
	return cols, rows
}

const (
	previewGap   = 2  // cells between thumbnails in the preview row
	previewCellW = 32 // PNG pixels per cell column (kitty scales to the cell box)
	previewCellH = 64 // PNG pixels per cell row
)

// normalizeImage scales and recompresses an image the way OpenCode
// does: fit within maxImageDim, then shrink until the base64 payload
// is at most maxImageBase64Bytes. Images that already fit are returned
// unchanged.
func normalizeImage(data []byte) ([]byte, string, error) {
	mime := sniffImageMIME(data)
	if mime == "" {
		return nil, "", fmt.Errorf("unrecognized image format")
	}
	src, _, err := image.Decode(bytes.NewReader(data))
	if err != nil {
		if base64.StdEncoding.EncodedLen(len(data)) <= maxImageBase64Bytes {
			return data, mime, nil
		}
		return nil, "", err
	}
	w, h := src.Bounds().Dx(), src.Bounds().Dy()
	if w <= maxImageDim && h <= maxImageDim && base64.StdEncoding.EncodedLen(len(data)) <= maxImageBase64Bytes {
		return data, mime, nil
	}
	scale := 1.0
	if w > maxImageDim || h > maxImageDim {
		sw := float64(maxImageDim) / float64(w)
		sh := float64(maxImageDim) / float64(h)
		if sw < sh {
			scale = sw
		} else {
			scale = sh
		}
	}
	for {
		nw := int(math.Round(float64(w) * scale))
		nh := int(math.Round(float64(h) * scale))
		if nw < 1 {
			nw = 1
		}
		if nh < 1 {
			nh = 1
		}
		dst := src
		if nw != w || nh != h {
			rgba := image.NewRGBA(image.Rect(0, 0, nw, nh))
			scaleBilinear(rgba, src)
			dst = rgba
		}
		var buf bytes.Buffer
		if err := png.Encode(&buf, dst); err != nil {
			return nil, "", err
		}
		out := buf.Bytes()
		outMIME := "image/png"
		if base64.StdEncoding.EncodedLen(len(out)) > maxImageBase64Bytes {
			buf.Reset()
			if err := jpeg.Encode(&buf, dst, &jpeg.Options{Quality: 80}); err == nil {
				out = buf.Bytes()
				outMIME = "image/jpeg"
			}
		}
		if base64.StdEncoding.EncodedLen(len(out)) <= maxImageBase64Bytes {
			return out, outMIME, nil
		}
		scale *= 0.8
		if nw <= 32 || nh <= 32 {
			return nil, "", fmt.Errorf("image too large after resize")
		}
	}
}

// digitFont is a 5x7 bitmap font for 0-9, one glyph per digit. The badge
// is drawn with it at 2x scale, which stays legible at preview size.
var digitFont = [10][7]string{
	{"01110", "10001", "10011", "10101", "11001", "10001", "01110"}, // 0
	{"00100", "01100", "00100", "00100", "00100", "00100", "01110"}, // 1
	{"01110", "10001", "00001", "00110", "01000", "10000", "11111"}, // 2
	{"11111", "00010", "00100", "00110", "00001", "10001", "01110"}, // 3
	{"00010", "00110", "01010", "10010", "11111", "00010", "00010"}, // 4
	{"11111", "10000", "11110", "00001", "00001", "10001", "01110"}, // 5
	{"00110", "01000", "10000", "11110", "10001", "10001", "01110"}, // 6
	{"11111", "00001", "00010", "00100", "01000", "01000", "01000"}, // 7
	{"01110", "10001", "10001", "01110", "10001", "10001", "01110"}, // 8
	{"01110", "10001", "10001", "01111", "00001", "00010", "01100"}, // 9
}

// makePreviewPNG builds the preview image for one pasted image: a
// high-resolution thumbnail that kitty scales into the cell box, with
// the image's number in an ultra-light-blue badge at the bottom-left.
func makePreviewPNG(data []byte, num int) ([]byte, error) {
	src, _, err := image.Decode(bytes.NewReader(data))
	if err != nil {
		return nil, err
	}
	sb := src.Bounds()
	cols, rows := previewBox(sb.Dx(), sb.Dy())
	dst := image.NewRGBA(image.Rect(0, 0, cols*previewCellW, rows*previewCellH))
	scaleImage(dst, src)
	drawBadge(dst, num)
	var buf bytes.Buffer
	if err := png.Encode(&buf, dst); err != nil {
		return nil, err
	}
	return buf.Bytes(), nil
}

// scaleImage resizes src into dst. Downscales with area averaging
// (avoids the sparkly bilinear aliasing); upscales with bilinear.
func scaleImage(dst *image.RGBA, src image.Image) {
	sb := src.Bounds()
	db := dst.Bounds()
	if sb.Dx() >= db.Dx() && sb.Dy() >= db.Dy() {
		scaleArea(dst, src)
		return
	}
	scaleBilinear(dst, src)
}

// scaleArea resizes src into dst by averaging the source pixels that
// cover each destination pixel.
func scaleArea(dst *image.RGBA, src image.Image) {
	sb := src.Bounds()
	sw, sh := sb.Dx(), sb.Dy()
	b := dst.Bounds()
	dw, dh := b.Dx(), b.Dy()
	for y := 0; y < dh; y++ {
		sy0 := float64(y) * float64(sh) / float64(dh)
		sy1 := float64(y+1) * float64(sh) / float64(dh)
		for x := 0; x < dw; x++ {
			sx0 := float64(x) * float64(sw) / float64(dw)
			sx1 := float64(x+1) * float64(sw) / float64(dw)
			dst.SetRGBA(b.Min.X+x, b.Min.Y+y, averageRect(src, sb, sx0, sy0, sx1, sy1))
		}
	}
}

func averageRect(src image.Image, sb image.Rectangle, x0, y0, x1, y1 float64) color.RGBA {
	ix0, iy0 := int(math.Floor(x0)), int(math.Floor(y0))
	ix1, iy1 := int(math.Ceil(x1))-1, int(math.Ceil(y1))-1
	if ix0 < sb.Min.X {
		ix0 = sb.Min.X
	}
	if iy0 < sb.Min.Y {
		iy0 = sb.Min.Y
	}
	if ix1 > sb.Max.X-1 {
		ix1 = sb.Max.X - 1
	}
	if iy1 > sb.Max.Y-1 {
		iy1 = sb.Max.Y - 1
	}
	if ix1 < ix0 || iy1 < iy0 {
		return rgbaOf(src.At(ix0, iy0))
	}
	var r, g, b, a float64
	var wsum float64
	for y := iy0; y <= iy1; y++ {
		yw := 1.0
		if y == iy0 {
			yw = 1 - (y0 - float64(y))
		}
		if y == iy1 {
			yw = math.Min(yw, y1-float64(y))
		}
		if yw <= 0 {
			continue
		}
		for x := ix0; x <= ix1; x++ {
			xw := 1.0
			if x == ix0 {
				xw = 1 - (x0 - float64(x))
			}
			if x == ix1 {
				xw = math.Min(xw, x1-float64(x))
			}
			if xw <= 0 {
				continue
			}
			w := xw * yw
			c := rgbaOf(src.At(x, y))
			r += float64(c.R) * w
			g += float64(c.G) * w
			b += float64(c.B) * w
			a += float64(c.A) * w
			wsum += w
		}
	}
	if wsum <= 0 {
		return color.RGBA{0, 0, 0, 255}
	}
	return color.RGBA{
		R: uint8(r/wsum + 0.5),
		G: uint8(g/wsum + 0.5),
		B: uint8(b/wsum + 0.5),
		A: uint8(a/wsum + 0.5),
	}
}

// scaleBilinear resizes src into dst with bilinear filtering. The source
// is stretched to fill the whole destination; callers pick a destination
// with the same aspect so this is a pure scale.
func scaleBilinear(dst *image.RGBA, src image.Image) {
	sb := src.Bounds()
	sw, sh := sb.Dx(), sb.Dy()
	b := dst.Bounds()
	for y := b.Min.Y; y < b.Max.Y; y++ {
		sy := (float64(y-b.Min.Y)+0.5)*float64(sh)/float64(b.Dy()) - 0.5
		for x := b.Min.X; x < b.Max.X; x++ {
			sx := (float64(x-b.Min.X)+0.5)*float64(sw)/float64(b.Dx()) - 0.5
			dst.Set(x, y, bilinearAt(src, sb, sx, sy))
		}
	}
}

// bilinearAt samples the source image at floating-point coordinates,
// blending the four surrounding pixels.
func bilinearAt(src image.Image, sb image.Rectangle, sx, sy float64) color.Color {
	x0 := int(math.Floor(sx))
	y0 := int(math.Floor(sy))
	fx := sx - float64(x0)
	fy := sy - float64(y0)
	clampX := func(v int) int {
		if v < sb.Min.X {
			return sb.Min.X
		}
		if v > sb.Max.X-1 {
			return sb.Max.X - 1
		}
		return v
	}
	clampY := func(v int) int {
		if v < sb.Min.Y {
			return sb.Min.Y
		}
		if v > sb.Max.Y-1 {
			return sb.Max.Y - 1
		}
		return v
	}
	x0, x1 := clampX(x0), clampX(x0+1)
	y0, y1 := clampY(y0), clampY(y0+1)
	c00 := rgbaOf(src.At(x0, y0))
	c01 := rgbaOf(src.At(x0, y1))
	c10 := rgbaOf(src.At(x1, y0))
	c11 := rgbaOf(src.At(x1, y1))
	blend := func(a, b uint8, t float64) uint8 {
		return uint8(float64(a)*(1-t) + float64(b)*t)
	}
	return color.RGBA{
		R: blend(blend(c00.R, c10.R, fx), blend(c01.R, c11.R, fx), fy),
		G: blend(blend(c00.G, c10.G, fx), blend(c01.G, c11.G, fx), fy),
		B: blend(blend(c00.B, c10.B, fx), blend(c01.B, c11.B, fx), fy),
		A: blend(blend(c00.A, c10.A, fx), blend(c01.A, c11.A, fx), fy),
	}
}

// rgbaOf converts any color to its RGBA components.
func rgbaOf(c color.Color) color.RGBA {
	r, g, b, a := c.RGBA()
	return color.RGBA{uint8(r >> 8), uint8(g >> 8), uint8(b >> 8), uint8(a >> 8)}
}

// badgeBlue is the ultra-light blue used for the preview number, in the
// same spirit as the bright-blue ANSI accents the app uses elsewhere.
var badgeBlue = color.RGBA{0xa8, 0xdc, 0xff, 0xff}

// drawBadge paints the image number into the bottom-left corner of the
// preview: a translucent dark box with an ultra-light-blue digit,
// scaled to the high-res thumbnail.
func drawBadge(dst *image.RGBA, num int) {
	digits := strconv.Itoa(num)
	scale := previewCellW / 4
	if scale < 2 {
		scale = 2
	}
	pad := scale + 1
	gw, gh := 5*scale, 7*scale
	badgeW := gw*len(digits) + pad*2
	badgeH := gh + pad*2
	b := dst.Bounds()
	x0, y0 := b.Min.X, b.Max.Y-badgeH

	// Translucent dark backing so the number reads over any image.
	for y := y0; y < b.Max.Y; y++ {
		for x := x0; x < x0+badgeW; x++ {
			c := dst.RGBAAt(x, y)
			c.R, c.G, c.B = c.R/3, c.G/3, c.B/3
			dst.SetRGBA(x, y, c)
		}
	}
	// The digits, in ultra light blue.
	for i, d := range digits {
		glyph := digitFont[d-'0']
		for gy, row := range glyph {
			for gx, ch := range row {
				if ch != '1' {
					continue
				}
				px := x0 + pad + i*gw + gx*scale
				py := y0 + pad + gy*scale
				for dy := 0; dy < scale; dy++ {
					for dx := 0; dx < scale; dx++ {
						dst.SetRGBA(px+dx, py+dy, badgeBlue)
					}
				}
			}
		}
	}
}

// --- painting to the terminal ---

// previewPlacement is one preview to transmit as a virtual kitty
// placement: the original image bytes, its 1-based marker number (also
// the kitty image id), and the cell box it occupies. Display is via
// Unicode placeholders in the View, not cursor-relative a=p placement.
type previewPlacement struct {
	data []byte
	num  int
	cols int
	rows int
}

// placeholder is the Kitty Unicode placeholder code point (U+10EEEE).
const placeholder = '\U0010EEEE'

// rowColDiacritics encode placeholder row/column (0..15) as combining
// marks, from kitty's rowcolumn-diacritics.txt (also used by chafa and
// notcurses). 0 = U+0305, 1 = U+030D, 2 = U+030E, ...
var rowColDiacritics = [16]rune{
	0x0305, 0x030D, 0x030E, 0x0310, 0x0312, 0x033D, 0x033E, 0x033F,
	0x0346, 0x034A, 0x034B, 0x034C, 0x0350, 0x0351, 0x0352, 0x0357,
}

const maxKittyPreviewID = 16

func rowColDiacritic(n int) rune {
	if n < 0 {
		n = 0
	}
	if n >= len(rowColDiacritics) {
		n = len(rowColDiacritics) - 1
	}
	return rowColDiacritics[n]
}

// placeholderGrid builds rows of cols U+10EEEE cells for kitty image id.
// Each cell has 256-color fg = id and explicit row+col combining
// diacritics. Color is reset at the end of each row.
func placeholderGrid(id, cols, rows int) string {
	if cols <= 0 || rows <= 0 {
		return ""
	}
	var sb strings.Builder
	fg := fmt.Sprintf("\x1b[38;5;%dm", id)
	for y := 0; y < rows; y++ {
		sb.WriteString(fg)
		for x := 0; x < cols; x++ {
			sb.WriteRune(placeholder)
			sb.WriteRune(rowColDiacritic(y))
			sb.WriteRune(rowColDiacritic(x))
		}
		sb.WriteString("\x1b[39m")
		if y < rows-1 {
			sb.WriteByte('\n')
		}
	}
	return sb.String()
}

func kittyDeleteVirtual(id int) string {
	return ansi.KittyGraphics(nil, "a=d", "d=I", fmt.Sprintf("i=%d", id), "q=2")
}

// kittyTerminal reports whether the current terminal speaks the Kitty
// graphics protocol, based on the usual environment signals.
func kittyTerminal() bool {
	prog := strings.ToLower(os.Getenv("TERM_PROGRAM"))
	term := strings.ToLower(os.Getenv("TERM"))
	switch {
	case strings.Contains(prog, "kitty"),
		strings.Contains(prog, "wezterm"),
		strings.Contains(prog, "ghostty"),
		strings.Contains(prog, "foot"),
		strings.Contains(prog, "konsole"),
		strings.Contains(prog, "contour"),
		strings.Contains(prog, "rio"):
		return true
	case strings.Contains(term, "kitty"):
		return true
	}
	return false
}

// kittyTransmit returns the chunked transmit sequences for one PNG.
// Payloads larger than the protocol's chunk limit are split with m=1 on
// every chunk except the last.
func kittyTransmit(id int, pngData []byte) string {
	b64 := base64.StdEncoding.EncodeToString(pngData)
	var sb strings.Builder
	opts := fmt.Sprintf("a=t,f=100,i=%d,q=2", id)
	for len(b64) > kittyMaxChunk {
		sb.WriteString(ansi.KittyGraphics(nil, opts+",m=1;"+b64[:kittyMaxChunk]))
		b64 = b64[kittyMaxChunk:]
	}
	sb.WriteString(ansi.KittyGraphics(nil, opts+";"+b64))
	return sb.String()
}

// paintKittyPreviews transmits virtual placements for the current
// pending set. Empty entries delete virtual images 1..16. Otherwise
// unused ids in that range are deleted, then each thumbnail is
// transmitted and virtually placed (U=1). No cursor motion, no CUP.
func paintKittyPreviews(entries []previewPlacement) {
	var sb strings.Builder
	used := map[int]bool{}
	for _, e := range entries {
		if e.num > 0 {
			used[e.num] = true
		}
	}
	for n := 1; n <= maxKittyPreviewID; n++ {
		if !used[n] {
			sb.WriteString(kittyDeleteVirtual(n))
		}
	}
	for _, e := range entries {
		if e.cols <= 0 || e.rows <= 0 {
			continue
		}
		pngData, err := makePreviewPNG(e.data, e.num)
		if err != nil {
			continue
		}
		sb.WriteString(kittyTransmit(e.num, pngData))
		sb.WriteString(ansi.KittyGraphics(nil,
			"a=p",
			"U=1",
			fmt.Sprintf("i=%d", e.num),
			fmt.Sprintf("c=%d", e.cols),
			fmt.Sprintf("r=%d", e.rows),
			"q=2",
		))
	}
	writeTTY(sb.String())
}
