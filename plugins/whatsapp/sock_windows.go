package main

import (
	"net"
	"time"

	"github.com/Microsoft/go-winio"
)

// RogueIM's local sockets are named pipes on Windows.
func dialHost(name string) (net.Conn, error) {
	timeout := 10 * time.Second
	return winio.DialPipe(`\\.\pipe\`+name, &timeout)
}
