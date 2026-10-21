package main

import (
	"bytes"
	"database/sql"
	"os"
	"path/filepath"
	"testing"
)

func TestMemDBIsSealedOnDisk(t *testing.T) {
	old := dataKey
	defer func() { dataKey = old }()
	dataKey = bytes.Repeat([]byte{9}, 32)
	path := filepath.Join(t.TempDir(), "whatsapp.db")

	db, err := openMemDB(path)
	if err != nil {
		t.Fatal(err)
	}
	if _, err := db.Exec("CREATE TABLE m (text TEXT); INSERT INTO m VALUES ('secret lunch plans')"); err != nil {
		t.Fatal(err)
	}
	if err := db.Close(); err != nil {
		t.Fatal(err)
	}
	disk, _ := os.ReadFile(path)
	if !isSealed(disk) || bytes.Contains(disk, []byte("secret lunch")) {
		t.Fatal("the database is readable on disk")
	}

	db, err = openMemDB(path)
	if err != nil {
		t.Fatal(err)
	}
	var text string
	if err := db.QueryRow("SELECT text FROM m").Scan(&text); err != nil || text != "secret lunch plans" {
		t.Fatal(text, err)
	}
	_ = db.Close()
}

// A plain database from before encryption, in WAL mode, comes over whole.
func TestMemDBTakesOverAPlainDatabase(t *testing.T) {
	old := dataKey
	defer func() { dataKey = old }()
	dataKey = bytes.Repeat([]byte{9}, 32)
	path := filepath.Join(t.TempDir(), "whatsapp.db")

	plain, err := sql.Open("sqlite", "file:"+path+"?_pragma=journal_mode(WAL)")
	if err != nil {
		t.Fatal(err)
	}
	plain.SetMaxOpenConns(1)
	if _, err := plain.Exec("CREATE TABLE m (text TEXT); INSERT INTO m VALUES ('from the wal')"); err != nil {
		t.Fatal(err)
	}
	_ = plain.Close()

	db, err := openMemDB(path)
	if err != nil {
		t.Fatal(err)
	}
	var text string
	if err := db.QueryRow("SELECT text FROM m").Scan(&text); err != nil || text != "from the wal" {
		t.Fatal(text, err)
	}
	_ = db.Close()
	disk, _ := os.ReadFile(path)
	if !isSealed(disk) {
		t.Fatal("not sealed after taking over")
	}
	if _, err := os.Stat(path + "-wal"); err == nil {
		t.Fatal("the plain WAL is still there")
	}
}
