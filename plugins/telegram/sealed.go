package main

// Plugin data at rest. RogueIM hands this plugin its own key in RIM_PLUGIN_KEY
// (hex); files are sealed with XChaCha20-Poly1305, the file name bound as
// associated data. Same format as rim_plugin_sdk::sealed (Rust), which the app
// also uses to show pictures: "RIMS1" | nonce[24] | ciphertext.
// (The same file is in plugins/whatsapp and plugins/telegram.)

import (
	"bytes"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"golang.org/x/crypto/chacha20poly1305"
)

const sealMagic = "RIMS1"

var dataKey = loadKey()

func loadKey() []byte {
	k, err := hex.DecodeString(strings.TrimSpace(os.Getenv("RIM_PLUGIN_KEY")))
	if err != nil || len(k) != chacha20poly1305.KeySize {
		return nil
	}
	return k
}

func isSealed(b []byte) bool {
	return len(b) >= len(sealMagic)+chacha20poly1305.NonceSizeX+chacha20poly1305.Overhead && string(b[:len(sealMagic)]) == sealMagic
}

func sealBytes(key []byte, name string, plain []byte) []byte {
	aead, _ := chacha20poly1305.NewX(key)
	nonce := make([]byte, chacha20poly1305.NonceSizeX)
	if _, err := rand.Read(nonce); err != nil {
		panic(err)
	}
	out := append([]byte(sealMagic), nonce...)
	return aead.Seal(out, nonce, plain, []byte(name))
}

func unsealBytes(key []byte, name string, data []byte) ([]byte, error) {
	if !isSealed(data) {
		return nil, errors.New("not a sealed file")
	}
	aead, err := chacha20poly1305.NewX(key)
	if err != nil {
		return nil, err
	}
	head := len(sealMagic) + chacha20poly1305.NonceSizeX
	plain, err := aead.Open(nil, data[len(sealMagic):head], data[head:], []byte(name))
	if err != nil {
		return nil, errors.New("wrong key or damaged file")
	}
	return plain, nil
}

// readSealed reads a plugin file. A file from before encryption is read as it
// is and sealed on the spot.
func readSealed(path string) ([]byte, error) {
	b, err := os.ReadFile(path)
	if err != nil || dataKey == nil {
		return b, err
	}
	if isSealed(b) {
		return unsealBytes(dataKey, filepath.Base(path), b)
	}
	_ = writeSealed(path, b)
	return b, nil
}

// writeSealed writes a plugin file, sealed when RogueIM gave a key (always, outside tests).
func writeSealed(path string, plain []byte) error {
	if dataKey == nil {
		return os.WriteFile(path, plain, 0o600)
	}
	tmp := path + ".sealing"
	f, err := os.OpenFile(tmp, os.O_WRONLY|os.O_CREATE|os.O_TRUNC, 0o600)
	if err != nil {
		return err
	}
	// Flushed before the rename, so a power loss cannot leave a file of zeros.
	_, err = f.Write(sealBytes(dataKey, filepath.Base(path), plain))
	if err == nil {
		err = f.Sync()
	}
	if cerr := f.Close(); err == nil {
		err = cerr
	}
	if err != nil {
		return err
	}
	return os.Rename(tmp, path)
}

// openDir holds plain copies handed to the system viewer ("open"); emptied
// when the plugin starts and stops.
func openDir() string {
	name := strings.TrimSuffix(filepath.Base(os.Args[0]), ".exe")
	return filepath.Join(os.TempDir(), name+"-open")
}

func clearOpenCopies() {
	_ = os.RemoveAll(openDir())
}

// plainCopy writes the decrypted media to dir under its own name (with a number
// if the name is taken there) and returns the new path.
func plainCopy(path, dir, name string) (string, error) {
	plain, err := readSealed(path)
	if err != nil {
		return "", err
	}
	if err := os.MkdirAll(dir, 0o700); err != nil {
		return "", err
	}
	if name == "" {
		name = filepath.Base(path)
	}
	out := filepath.Join(dir, name)
	ext := filepath.Ext(name)
	for i := 2; ; i++ {
		if old, err := os.ReadFile(out); err != nil || bytes.Equal(old, plain) {
			break
		}
		out = filepath.Join(dir, fmt.Sprintf("%s (%d)%s", strings.TrimSuffix(name, ext), i, ext))
	}
	return out, os.WriteFile(out, plain, 0o600)
}

// downloadsDir is where "save" puts a decrypted copy.
func downloadsDir() string {
	home, err := os.UserHomeDir()
	if err != nil {
		return os.TempDir()
	}
	return filepath.Join(home, "Downloads")
}
