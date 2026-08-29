package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestUnescapePastePath(t *testing.T) {
	tmp := t.TempDir()
	abs := filepath.Join(tmp, "shot.png")
	cases := []struct {
		in, want string
	}{
		{`'/tmp/foo.png'`, "/tmp/foo.png"},
		{`"/tmp/foo.png"`, "/tmp/foo.png"},
		{"file://" + abs, abs},
		{`/tmp/has\ space.png`, "/tmp/has space.png"},
	}
	for _, c := range cases {
		if got := unescapePastePath(c.in); got != c.want {
			t.Errorf("unescapePastePath(%q) = %q, want %q", c.in, got, c.want)
		}
	}
}

func TestLocalImagesFromPaste(t *testing.T) {
	dir := t.TempDir()
	png := []byte{0x89, 'P', 'N', 'G', 0x0D, 0x0A, 0x1A, 0x0A, 1, 2, 3}
	p1 := filepath.Join(dir, "a.png")
	p2 := filepath.Join(dir, "b.png")
	if err := os.WriteFile(p1, png, 0o600); err != nil {
		t.Fatal(err)
	}
	if err := os.WriteFile(p2, png, 0o600); err != nil {
		t.Fatal(err)
	}
	txt := filepath.Join(dir, "note.txt")
	if err := os.WriteFile(txt, []byte("hi"), 0o600); err != nil {
		t.Fatal(err)
	}

	got := localImagesFromPaste("'" + p1 + "'")
	if len(got) != 1 || got[0].name != "a.png" {
		t.Fatalf("single path: %+v", got)
	}

	got = localImagesFromPaste(p1 + "\n" + p2)
	if len(got) != 2 {
		t.Fatalf("two paths: got %d", len(got))
	}

	got = localImagesFromPaste("file://" + p1)
	if len(got) != 1 {
		t.Fatalf("file URL: %+v", got)
	}

	if localImagesFromPaste("hello "+p1) != nil {
		t.Fatal("mixed text+path should fall through as text")
	}
	if localImagesFromPaste(txt) != nil {
		t.Fatal("non-image file should fall through as text")
	}
	if localImagesFromPaste("just some text") != nil {
		t.Fatal("plain text should fall through")
	}
	if localImagesFromPaste("") != nil {
		t.Fatal("empty paste")
	}
	if localImagesFromPaste(strings.Repeat("x", 8)+".png") != nil {
		t.Fatal("missing file should fall through")
	}
}
