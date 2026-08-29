package main

import (
	"bytes"
	"encoding/base64"
	"image"
	"image/png"
	"strings"
	"testing"
)

func TestPreviewBoxIsSmall(t *testing.T) {
	c, r := previewBox(1920, 1080)
	if c > 8 || r > 3 {
		t.Fatalf("previewBox(1920,1080) = %dx%d, want at most 8x3", c, r)
	}
	c, r = previewBox(100, 100)
	if r > 3 || c > 8 {
		t.Fatalf("square preview too large: %dx%d", c, r)
	}
}

func TestNormalizeImageLeavesSmallPNG(t *testing.T) {
	img := image.NewRGBA(image.Rect(0, 0, 8, 8))
	var buf bytes.Buffer
	if err := png.Encode(&buf, img); err != nil {
		t.Fatal(err)
	}
	in := buf.Bytes()
	out, mime, err := normalizeImage(in)
	if err != nil {
		t.Fatal(err)
	}
	if mime != "image/png" {
		t.Fatalf("mime = %q", mime)
	}
	if !bytes.Equal(in, out) {
		t.Fatal("small png should pass through unchanged")
	}
}

func TestNormalizeImageFitsDimensionCap(t *testing.T) {
	img := image.NewRGBA(image.Rect(0, 0, 2400, 1200))
	var buf bytes.Buffer
	if err := png.Encode(&buf, img); err != nil {
		t.Fatal(err)
	}
	out, _, err := normalizeImage(buf.Bytes())
	if err != nil {
		t.Fatal(err)
	}
	w, h, err := imageSize(out)
	if err != nil {
		t.Fatal(err)
	}
	if w > maxImageDim || h > maxImageDim {
		t.Fatalf("normalized size %dx%d exceeds %d", w, h, maxImageDim)
	}
	if base64.StdEncoding.EncodedLen(len(out)) > maxImageBase64Bytes {
		t.Fatalf("base64 payload %d exceeds cap", base64.StdEncoding.EncodedLen(len(out)))
	}
}

func TestMakePreviewPNGIsHiRes(t *testing.T) {
	img := image.NewRGBA(image.Rect(0, 0, 200, 200))
	var buf bytes.Buffer
	if err := png.Encode(&buf, img); err != nil {
		t.Fatal(err)
	}
	out, err := makePreviewPNG(buf.Bytes(), 1)
	if err != nil {
		t.Fatal(err)
	}
	cfg, err := png.DecodeConfig(bytes.NewReader(out))
	if err != nil {
		t.Fatal(err)
	}
	c, r := previewBox(200, 200)
	if cfg.Width < c*previewCellW || cfg.Height < r*previewCellH {
		t.Fatalf("preview PNG %dx%d, want at least %dx%d", cfg.Width, cfg.Height, c*previewCellW, r*previewCellH)
	}
}

func TestPlaceholderGrid(t *testing.T) {
	s := placeholderGrid(42, 2, 2)
	if !strings.Contains(s, "\U0010EEEE") {
		t.Fatal("missing U+10EEEE placeholder")
	}
	if !strings.Contains(s, "\x1b[38;5;42m") {
		t.Fatal("missing 256-color fg 42")
	}
	if !strings.ContainsRune(s, '\u0305') {
		t.Fatal("missing U+0305 (row/col 0)")
	}
	if !strings.ContainsRune(s, '\u030D') {
		t.Fatal("missing U+030D (row/col 1)")
	}
	if !strings.Contains(s, "\x1b[39m") {
		t.Fatal("missing color reset")
	}
}

func testPNG(t *testing.T) []byte {
	t.Helper()
	img := image.NewRGBA(image.Rect(0, 0, 8, 8))
	var buf bytes.Buffer
	if err := png.Encode(&buf, img); err != nil {
		t.Fatal(err)
	}
	return buf.Bytes()
}

func captureTTY(t *testing.T, fn func()) string {
	t.Helper()
	var got strings.Builder
	orig := writeTTY
	writeTTY = func(s string) { got.WriteString(s) }
	t.Cleanup(func() { writeTTY = orig })
	fn()
	return got.String()
}

func TestPaintKittyPreviewsVirtualPlace(t *testing.T) {
	out := captureTTY(t, func() {
		paintKittyPreviews([]previewPlacement{{
			data: testPNG(t),
			num:  1,
			cols: 3,
			rows: 2,
		}})
	})
	if !strings.Contains(out, "U=1") {
		t.Fatal("missing U=1 virtual placement")
	}
	if !strings.Contains(out, "q=2") {
		t.Fatal("missing q=2 quiet mode")
	}
	if strings.Contains(out, "\x1b[") {
		t.Fatal("wrote CUP/CSI; virtual placements must not move the cursor")
	}
}

func TestPaintKittyPreviewsEmptyDeletesVirtual(t *testing.T) {
	out := captureTTY(t, func() {
		paintKittyPreviews(nil)
	})
	if !strings.Contains(out, "d=I") {
		t.Fatal("empty pending must delete virtual images with d=I")
	}
	if strings.Contains(out, "d=a") && !strings.Contains(out, "d=I") {
		t.Fatal("d=a alone does not delete virtual placements")
	}
}
