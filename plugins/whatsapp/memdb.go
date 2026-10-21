package main

// The WhatsApp database (session keys and chats) lives in memory; on disk
// there is only its sealed copy, whatsapp.db, written a few seconds after each
// change and when the plugin stops.

import (
	"context"
	"database/sql"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"sync"
	"time"
)

type memDB struct {
	*sql.DB
	path    string
	mu      sync.Mutex
	changes int64
	stop    chan struct{}
	done    chan struct{}
}

type serializer interface {
	Serialize() ([]byte, error)
	Deserialize([]byte) error
}

func openMemDB(path string) (*memDB, error) {
	image, err := loadDBImage(path)
	if err != nil {
		return nil, err
	}
	db, err := sql.Open("sqlite", "file:whatsapp?mode=memory&_pragma=foreign_keys(1)")
	if err != nil {
		return nil, err
	}
	// One connection: an in-memory database belongs to the connection that holds it.
	db.SetMaxOpenConns(1)
	db.SetMaxIdleConns(1)
	db.SetConnMaxLifetime(0)
	db.SetConnMaxIdleTime(0)
	m := &memDB{DB: db, path: path, stop: make(chan struct{}), done: make(chan struct{})}
	if image != nil {
		if err := m.raw(func(s serializer) error { return s.Deserialize(image) }); err != nil {
			db.Close()
			return nil, fmt.Errorf("restore: %w", err)
		}
		// Deserialize leaves foreign keys as the DSN set them; make sure.
		_, _ = db.Exec("PRAGMA foreign_keys = ON")
	}
	m.changes = m.totalChanges()
	go m.loop()
	return m, nil
}

func (m *memDB) raw(f func(serializer) error) error {
	c, err := m.DB.Conn(context.Background())
	if err != nil {
		return err
	}
	defer c.Close()
	return c.Raw(func(dc any) error {
		s, ok := dc.(serializer)
		if !ok {
			return errors.New("sqlite driver cannot serialize")
		}
		return f(s)
	})
}

func (m *memDB) totalChanges() int64 {
	var n int64
	_ = m.DB.QueryRow("SELECT total_changes()").Scan(&n)
	return n
}

// save writes the sealed copy if anything changed since the last one.
func (m *memDB) save(force bool) error {
	m.mu.Lock()
	defer m.mu.Unlock()
	n := m.totalChanges()
	if n == m.changes && !force {
		return nil
	}
	var image []byte
	if err := m.raw(func(s serializer) (err error) { image, err = s.Serialize(); return }); err != nil {
		return err
	}
	if err := writeSealed(m.path, image); err != nil {
		return err
	}
	m.changes = n
	return nil
}

func (m *memDB) loop() {
	defer close(m.done)
	t := time.NewTicker(3 * time.Second)
	defer t.Stop()
	for {
		select {
		case <-m.stop:
			return
		case <-t.C:
			_ = m.save(false)
		}
	}
}

// Close writes the last state and closes the database.
func (m *memDB) Close() error {
	close(m.stop)
	<-m.done
	err := m.save(false)
	return errors.Join(err, m.DB.Close())
}

// loadDBImage returns the database bytes from the sealed file; a database from
// before encryption (a plain SQLite file, perhaps with a WAL) is folded into
// one image first, and its plain files are removed once sealed.
func loadDBImage(path string) ([]byte, error) {
	b, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return nil, nil
	}
	if err != nil {
		return nil, err
	}
	if isSealed(b) {
		if dataKey == nil {
			return nil, errors.New("the database is encrypted, but RogueIM gave no key")
		}
		return unsealBytes(dataKey, filepath.Base(path), b)
	}
	// Plain SQLite from an older version: checkpoint the WAL into the file and
	// switch it to a rollback journal (an in-memory copy cannot use WAL).
	old, err := sql.Open("sqlite", "file:"+path)
	if err != nil {
		return nil, err
	}
	_, _ = old.Exec("PRAGMA wal_checkpoint(TRUNCATE)")
	_, _ = old.Exec("PRAGMA journal_mode = DELETE")
	old.Close()
	if b, err = os.ReadFile(path); err != nil {
		return nil, err
	}
	if dataKey != nil {
		if err := writeSealed(path, b); err != nil {
			return nil, err
		}
		_ = os.Remove(path + "-wal")
		_ = os.Remove(path + "-shm")
	}
	return b, nil
}
