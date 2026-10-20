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

	"github.com/Microsoft/go-winio"
	"go.mau.fi/whatsmeow/proto/waE2E"
	"google.golang.org/protobuf/proto"
)

func listen(t *testing.T, name string) net.Listener {
	var l net.Listener
	var err error
	switch runtime.GOOS {
	case "windows":
		l, err = winio.ListenPipe(`\\.\pipe\`+name, nil)
	case "linux":
		l, err = net.Listen("unix", "@"+name)
	default:
		l, err = net.Listen("unix", "/tmp/"+name)
	}
	if err != nil {
		t.Fatal(err)
	}
	return l
}

// Started like RogueIM starts it, the plugin says what it is and asks first:
// before the warning is accepted it announces no connection and makes none.
func TestAsksBeforeItConnects(t *testing.T) {
	bin := filepath.Join(t.TempDir(), "rim-plugin-whatsapp"+map[bool]string{true: ".exe"}[runtime.GOOS == "windows"])
	if out, err := exec.Command("go", "build", "-o", bin, ".").CombinedOutput(); err != nil {
		t.Fatalf("build: %v\n%s", err, out)
	}
	name := fmt.Sprintf("rogueim-test-whatsapp-%d", os.Getpid())
	l := listen(t, name)
	defer l.Close()
	cmd := exec.Command(bin)
	cmd.Env = append(os.Environ(), "RIM_PLUGIN_SOCKET="+name, "RIM_PLUGIN_DATA="+t.TempDir())
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	defer cmd.Process.Kill()
	conn, err := l.Accept()
	if err != nil {
		t.Fatal(err)
	}
	_ = conn.SetReadDeadline(time.Now().Add(15 * time.Second))
	sc := bufio.NewScanner(conn)
	sc.Buffer(make([]byte, 1<<20), 1<<20)
	var got []map[string]any
	for sc.Scan() {
		var m map[string]any
		if err := json.Unmarshal(sc.Bytes(), &m); err != nil {
			t.Fatal(err)
		}
		got = append(got, m)
		if m["t"] == "view" {
			break
		}
	}
	if len(got) < 3 || got[0]["t"] != "hello" || got[0]["id"] != "whatsapp" {
		t.Fatalf("expected hello first: %v", got)
	}
	var channel, view map[string]any
	for _, m := range got {
		switch m["t"] {
		case "channel":
			channel = m
		case "view":
			view = m
		case "network":
			t.Fatalf("announced a connection before the warning was accepted: %v", m)
		}
	}
	if channel["level"] != "C" || channel["files"] != true || !strings.Contains(channel["warning"].(string), "unofficial") {
		t.Fatalf("channel: %v", channel)
	}
	b, _ := json.Marshal(view)
	if !strings.Contains(string(b), `"id":"accept"`) || !strings.Contains(string(b), "block your number") {
		t.Fatalf("no warning with an accept button: %s", b)
	}
	_, _ = conn.Write([]byte(`{"t":"stop"}` + "\n"))
	done := make(chan error, 1)
	go func() { done <- cmd.Wait() }()
	select {
	case <-done:
	case <-time.After(10 * time.Second):
		t.Fatal("did not stop")
	}
}

func TestMediaBecomesLinesWithLinks(t *testing.T) {
	cases := []struct {
		msg        *waE2E.Message
		text, kind string
	}{
		{&waE2E.Message{Conversation: proto.String("hi")}, "hi", ""},
		{&waE2E.Message{AudioMessage: &waE2E.AudioMessage{PTT: proto.Bool(true), Seconds: proto.Uint32(72)}}, "[voice message 1:12]", "audio"},
		{&waE2E.Message{DocumentMessage: &waE2E.DocumentMessage{FileName: proto.String("plan.pdf")}}, "[file: plan.pdf]", "doc"},
		{&waE2E.Message{ImageMessage: &waE2E.ImageMessage{Caption: proto.String("beach")}}, "beach", "image"},
		{&waE2E.Message{StickerMessage: &waE2E.StickerMessage{}}, "[sticker]", "sticker"},
		{&waE2E.Message{LocationMessage: &waE2E.LocationMessage{DegreesLatitude: proto.Float64(47.85), DegreesLongitude: proto.Float64(12.12)}}, "[location 47.85000, 12.12000]", "location"},
	}
	for _, c := range cases {
		text, kind := describe(c.msg)
		if text != c.text || kind != c.kind {
			t.Errorf("got %q %q, want %q %q", text, kind, c.text, c.kind)
		}
	}
}

// Live, only with RIM_WA_LIVE=1: after "accept" a QR code comes from WhatsApp.
// Nothing is linked unless a phone scans it.
func TestQRCodeFromWhatsApp(t *testing.T) {
	if os.Getenv("RIM_WA_LIVE") != "1" {
		t.Skip("set RIM_WA_LIVE=1 to talk to WhatsApp's servers")
	}
	bin := filepath.Join(t.TempDir(), "rim-plugin-whatsapp"+map[bool]string{true: ".exe"}[runtime.GOOS == "windows"])
	if out, err := exec.Command("go", "build", "-o", bin, ".").CombinedOutput(); err != nil {
		t.Fatalf("build: %v\n%s", err, out)
	}
	name := fmt.Sprintf("rogueim-test-whatsapp-live-%d", os.Getpid())
	l := listen(t, name)
	defer l.Close()
	cmd := exec.Command(bin)
	cmd.Env = append(os.Environ(), "RIM_PLUGIN_SOCKET="+name, "RIM_PLUGIN_DATA="+t.TempDir())
	if err := cmd.Start(); err != nil {
		t.Fatal(err)
	}
	defer cmd.Process.Kill()
	conn, err := l.Accept()
	if err != nil {
		t.Fatal(err)
	}
	_, _ = conn.Write([]byte(`{"t":"click","id":"accept"}` + "\n"))
	_ = conn.SetReadDeadline(time.Now().Add(45 * time.Second))
	sc := bufio.NewScanner(conn)
	sc.Buffer(make([]byte, 1<<20), 1<<20)
	for sc.Scan() {
		line := sc.Text()
		if strings.Contains(line, `"kind":"qr"`) {
			var v struct{ Items []Item }
			_ = json.Unmarshal([]byte(line), &v)
			for _, it := range v.Items {
				if it.Kind == "qr" && strings.Count(it.Text, ",") >= 3 {
					t.Logf("QR payload: %d characters", len(it.Text))
					_, _ = conn.Write([]byte(`{"t":"stop"}` + "\n"))
					return
				}
			}
		}
	}
	t.Fatal("no QR code from WhatsApp")
}
