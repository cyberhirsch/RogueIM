//go:build !windows

package main

import (
	"net"
	"runtime"
)

// Linux: the abstract namespace; other Unix systems: a socket file in /tmp
// (what the Rust side, interprocess' GenericNamespaced, uses).
func dialHost(name string) (net.Conn, error) {
	if runtime.GOOS == "linux" {
		return net.Dial("unix", "@"+name)
	}
	return net.Dial("unix", "/tmp/"+name)
}

// listenHost is the RogueIM side, for the tests.
func listenHost(name string) (net.Listener, error) {
	if runtime.GOOS == "linux" {
		return net.Listen("unix", "@"+name)
	}
	return net.Listen("unix", "/tmp/"+name)
}
