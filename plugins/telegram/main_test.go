package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"net"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"

	"github.com/gotd/td/tg"
)

func listen(t *testing.T, name string) net.Listener {
	l, err := listenHost(name)
	if err != nil {
		t.Fatal(err)
	}
	return l
}

// plugin starts the plugin like RogueIM does and returns its messages.
func plugin(t *testing.T, data string) (net.Conn, <-chan map[string]any, *exec.Cmd) {
	bin := filepath.Join(t.TempDir(), "rim-plugin-telegram"+map[bool]string{true: ".exe"}[runtime.GOOS == "windows"])
	if out, err := exec.Command("go", "build", "-o", bin, ".").CombinedOutput(); err != nil {
		t.Fatalf("build: %v\n%s", err, out)
	}
	name := fmt.Sprintf("rogueim-test-telegram-%d-%s", os.Getpid(), t.Name())
	l := listen(t, name)
	t.Cleanup(func() { l.Close() })
	cmd := exec.Command(bin)
	cmd.Env = append(os.Environ(), "RIM_PLUGIN_SOCKET="+name, "RIM_PLUGIN_DATA="+data)
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = cmd.Process.Kill() })
	conn, err := l.Accept()
	if err != nil {
		t.Fatal(err)
	}
	ch := make(chan map[string]any, 64)
	go func() {
		sc := bufio.NewScanner(conn)
		sc.Buffer(make([]byte, 1<<20), 1<<20)
		for sc.Scan() {
			var m map[string]any
			if json.Unmarshal(sc.Bytes(), &m) == nil {
				ch <- m
			}
		}
		close(ch)
	}()
	return conn, ch, cmd
}

func wait(t *testing.T, ch <-chan map[string]any, secs int, what string, f func(m map[string]any, raw string) bool) map[string]any {
	t.Helper()
	end := time.After(time.Duration(secs) * time.Second)
	for {
		select {
		case m, ok := <-ch:
			if !ok {
				t.Fatalf("plugin ended before %s", what)
			}
			b, _ := json.Marshal(m)
			if f(m, string(b)) {
				return m
			}
		case <-end:
			t.Fatalf("no %s", what)
		}
	}
}

func stop(t *testing.T, conn net.Conn, cmd *exec.Cmd) {
	_, _ = conn.Write([]byte(`{"t":"stop"}` + "\n"))
	done := make(chan error, 1)
	go func() { done <- cmd.Wait() }()
	select {
	case <-done:
	case <-time.After(10 * time.Second):
		t.Fatal("did not stop")
	}
}

// Without an API key the plugin says what it is, asks for the key and
// connects nowhere.
func TestAsksForTheKeyFirst(t *testing.T) {
	conn, ch, cmd := plugin(t, t.TempDir())
	hello := wait(t, ch, 15, "hello", func(m map[string]any, _ string) bool { return true })
	if hello["t"] != "hello" || hello["id"] != "telegram" {
		t.Fatalf("expected hello first: %v", hello)
	}
	var channel map[string]any
	wait(t, ch, 15, "the key form", func(m map[string]any, raw string) bool {
		switch m["t"] {
		case "channel":
			channel = m
		case "network":
			t.Fatalf("announced a connection without a key: %v", m)
		}
		return m["t"] == "view" && strings.Contains(raw, `"id":"api_id"`) && strings.Contains(raw, `"id":"api_hash"`)
	})
	if channel["level"] != "D" || channel["files"] != true || !strings.Contains(channel["warning"].(string), "not end-to-end encrypted") {
		t.Fatalf("channel: %v", channel)
	}
	stop(t, conn, cmd)
}

// With RIM_TG_LIVE=1: a made-up key reaches Telegram, which turns it down.
// That proves the whole way there (MTProto, key exchange, login request).
func TestTelegramTurnsDownAWrongKey(t *testing.T) {
	if os.Getenv("RIM_TG_LIVE") == "" {
		t.Skip("talks to Telegram; set RIM_TG_LIVE=1")
	}
	conn, ch, cmd := plugin(t, t.TempDir())
	wait(t, ch, 15, "the key form", func(m map[string]any, raw string) bool { return strings.Contains(raw, `"id":"api_hash"`) })
	_, _ = conn.Write([]byte(`{"t":"input","id":"api_id","value":"12345"}` + "\n"))
	_, _ = conn.Write([]byte(`{"t":"input","id":"api_hash","value":"0123456789abcdef0123456789abcdef"}` + "\n"))
	wait(t, ch, 60, "Telegram's answer", func(m map[string]any, raw string) bool {
		if m["t"] == "view" && strings.Contains(raw, "tg://login") {
			t.Fatalf("Telegram took a made-up key: %s", raw)
		}
		return m["t"] == "view" && strings.Contains(raw, "does not know this api_id")
	})
	stop(t, conn, cmd)
}

func TestMediaBecomesLinesWithLinks(t *testing.T) {
	cases := []struct {
		m          *tg.Message
		text, kind string
	}{
		{&tg.Message{Message: "hi"}, "hi", ""},
		{&tg.Message{Message: "beach", Media: &tg.MessageMediaPhoto{}}, "beach", "photo"},
		{&tg.Message{Media: &tg.MessageMediaDocument{Document: &tg.Document{Attributes: []tg.DocumentAttributeClass{&tg.DocumentAttributeAudio{Voice: true, Duration: 72}}}}}, "[voice message 1:12]", "audio"},
		{&tg.Message{Media: &tg.MessageMediaDocument{Document: &tg.Document{Attributes: []tg.DocumentAttributeClass{&tg.DocumentAttributeFilename{FileName: "plan.pdf"}}}}}, "[file: plan.pdf]", "doc"},
		{&tg.Message{Media: &tg.MessageMediaDocument{Document: &tg.Document{Attributes: []tg.DocumentAttributeClass{&tg.DocumentAttributeSticker{Alt: "😀"}}}}}, "[sticker 😀]", ""},
		{&tg.Message{Media: &tg.MessageMediaGeo{Geo: &tg.GeoPoint{Lat: 47.85, Long: 12.12}}}, "[location 47.85000, 12.12000]", "location"},
	}
	for _, c := range cases {
		text, kind := describe(c.m)
		if text != c.text || kind != c.kind {
			t.Errorf("got %q %q, want %q %q", text, kind, c.text, c.kind)
		}
	}
}

func TestBiggestPhotoUpTo1280(t *testing.T) {
	md := &tg.MessageMediaPhoto{Photo: &tg.Photo{Sizes: []tg.PhotoSizeClass{
		&tg.PhotoStrippedSize{Type: "i"},
		&tg.PhotoSize{Type: "m", W: 320, H: 240, Size: 10},
		&tg.PhotoSizeProgressive{Type: "y", W: 1280, H: 960, Sizes: []int{50, 90}},
		&tg.PhotoSize{Type: "w", W: 2560, H: 1920, Size: 400},
	}}}
	if typ, n := photoSize(md); typ != "y" || n != 90 {
		t.Fatalf("got %q %d", typ, n)
	}
}
