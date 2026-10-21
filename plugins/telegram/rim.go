// The RogueIM plugin protocol (crates/rim-plugin-sdk): JSON lines over a
// local socket whose name RogueIM passes in RIM_PLUGIN_SOCKET.
package main

import (
	"bufio"
	"encoding/json"
	"fmt"
	"net"
	"os"
	"sync"
)

type Item struct {
	Kind    string  `json:"kind"`
	ID      string  `json:"id"`
	Text    string  `json:"text"`
	Hint    string  `json:"hint"`
	Value   float32 `json:"value"`
	Checked bool    `json:"checked"`
	Row     uint32  `json:"row"`
}

func text(t string, row uint32) Item       { return Item{Kind: "text", Text: t, Row: row} }
func dim(t string, row uint32) Item        { return Item{Kind: "dim", Text: t, Row: row} }
func button(id, t string, row uint32) Item { return Item{Kind: "button", ID: id, Text: t, Row: row} }
func qr(payload string, row uint32) Item   { return Item{Kind: "qr", Text: payload, Row: row} }
func input(id, hint string, row uint32) Item {
	return Item{Kind: "input", ID: id, Hint: hint, Row: row}
}

type NetContact struct {
	ID     string `json:"id"`
	Name   string `json:"name"`
	Status string `json:"status"`
	Info   string `json:"info"`
	Unread uint32 `json:"unread"`
}

type NetAction struct {
	ID    string `json:"id"`
	Label string `json:"label"`
}

type NetLine struct {
	ID      string      `json:"id"`
	TS      int64       `json:"ts"`
	FromMe  bool        `json:"from_me"`
	Who     string      `json:"who"`
	Text    string      `json:"text"`
	State   string      `json:"state"`
	Image   string      `json:"image,omitempty"`
	Actions []NetAction `json:"actions,omitempty"`
}

// A message from RogueIM: "t" names it, the other fields depend on it.
type FromHost struct {
	T       string `json:"t"`
	ID      string `json:"id"`
	Value   string `json:"value"`
	Chat    string `json:"chat"`
	Text    string `json:"text"`
	Line    string `json:"line"`
	Path    string `json:"path"`
	Address string `json:"address"`
}

type Host struct {
	mu   sync.Mutex
	conn net.Conn
	In   chan FromHost
}

func connectHost(id string) (*Host, error) {
	name := os.Getenv("RIM_PLUGIN_SOCKET")
	if name == "" {
		return nil, fmt.Errorf("RIM_PLUGIN_SOCKET not set: plugins are started by RogueIM")
	}
	conn, err := dialHost(name)
	if err != nil {
		return nil, err
	}
	h := &Host{conn: conn, In: make(chan FromHost, 64)}
	go func() {
		sc := bufio.NewScanner(conn)
		sc.Buffer(make([]byte, 64*1024), 16*1024*1024)
		for sc.Scan() {
			var m FromHost
			if json.Unmarshal(sc.Bytes(), &m) == nil {
				h.In <- m
			}
		}
		h.In <- FromHost{T: "stop"}
	}()
	h.Send(map[string]any{"t": "hello", "id": id})
	return h, nil
}

func (h *Host) Send(m map[string]any) {
	b, err := json.Marshal(m)
	if err != nil {
		return
	}
	h.mu.Lock()
	defer h.mu.Unlock()
	_, _ = h.conn.Write(append(b, '\n'))
}

func (h *Host) View(items []Item) { h.Send(map[string]any{"t": "view", "items": items}) }
