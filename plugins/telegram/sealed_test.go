package main

import (
	"bytes"
	"encoding/hex"
	"os"
	"path/filepath"
	"testing"

	"golang.org/x/crypto/chacha20poly1305"
)

// The same bytes as rim_plugin_sdk::sealed::tests::known_answer (Rust).
func TestSealedKnownAnswer(t *testing.T) {
	aead, _ := chacha20poly1305.NewX(bytes.Repeat([]byte{0x11}, 32))
	ct := aead.Seal(nil, bytes.Repeat([]byte{0x22}, 24), []byte("rim"), []byte("a.json"))
	if got := hex.EncodeToString(ct); got != "f10e8a369001e04cf03d0880b81b3958d6b2d1" {
		t.Fatalf("got %s", got)
	}
}

func TestSealedFiles(t *testing.T) {
	old := dataKey
	defer func() { dataKey = old }()
	dataKey = bytes.Repeat([]byte{7}, 32)
	dir := t.TempDir()
	path := filepath.Join(dir, "chats.json")
	// A file from before encryption is read and sealed on the spot.
	_ = os.WriteFile(path, []byte(`{"a":1}`), 0o600)
	if b, err := readSealed(path); err != nil || string(b) != `{"a":1}` {
		t.Fatal(string(b), err)
	}
	disk, _ := os.ReadFile(path)
	if !isSealed(disk) || bytes.Contains(disk, []byte(`"a"`)) {
		t.Fatal("not sealed on disk")
	}
	if b, err := readSealed(path); err != nil || string(b) != `{"a":1}` {
		t.Fatal(string(b), err)
	}
	// Bound to the name: renamed, it does not open.
	_ = os.Rename(path, filepath.Join(dir, "other.json"))
	if _, err := readSealed(filepath.Join(dir, "other.json")); err == nil {
		t.Fatal("a renamed file opened")
	}
	// A plain copy for the viewer.
	_ = writeSealed(path, []byte("pic"))
	out, err := plainCopy(path, filepath.Join(dir, "open"), "")
	if b, _ := os.ReadFile(out); err != nil || string(b) != "pic" {
		t.Fatal(out, err)
	}
}
