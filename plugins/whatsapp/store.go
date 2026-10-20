// Chats and messages as RogueIM shows them. whatsmeow keeps keys and
// contacts but no messages, so the plugin keeps its own copy, in the same
// SQLite file, in the plugin's folder.
package main

import (
	"database/sql"
)

type Msg struct {
	Chat   string
	ID     string
	TS     int64
	FromMe bool
	Sender string // JID, for read receipts in groups
	Who    string
	Text   string
	Kind   string // "", "image", "video", "audio", "doc", "location", "sticker"
	Media  []byte // the WhatsApp message, to download its media later
	File   string // the downloaded media, once there is one
	State  string // own messages: "sending", "sent", "failed"
}

type Chat struct {
	JID     string
	Name    string
	IsGroup bool
	LastTS  int64
	Unread  int
}

type Store struct{ db *sql.DB }

func openStore(db *sql.DB) (*Store, error) {
	_, err := db.Exec(`
CREATE TABLE IF NOT EXISTS rim_chat (
	jid TEXT PRIMARY KEY, name TEXT NOT NULL DEFAULT '', is_group INTEGER NOT NULL DEFAULT 0,
	last_ts INTEGER NOT NULL DEFAULT 0, read_ts INTEGER NOT NULL DEFAULT 0, hidden INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS rim_msg (
	chat TEXT NOT NULL, id TEXT NOT NULL, ts INTEGER NOT NULL, from_me INTEGER NOT NULL, sender TEXT NOT NULL DEFAULT '',
	who TEXT NOT NULL DEFAULT '', text TEXT NOT NULL DEFAULT '', kind TEXT NOT NULL DEFAULT '', media BLOB,
	file TEXT NOT NULL DEFAULT '', state TEXT NOT NULL DEFAULT '', PRIMARY KEY (chat, id));
CREATE INDEX IF NOT EXISTS rim_msg_ts ON rim_msg (chat, ts);`)
	return &Store{db}, err
}

// Put stores a message; a known one keeps its downloaded file.
func (s *Store) Put(m Msg, isGroup bool) error {
	_, err := s.db.Exec(`INSERT INTO rim_msg (chat, id, ts, from_me, sender, who, text, kind, media, file, state)
		VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
		ON CONFLICT (chat, id) DO UPDATE SET text = excluded.text, state = excluded.state, who = excluded.who,
		media = COALESCE(excluded.media, rim_msg.media), kind = excluded.kind`,
		m.Chat, m.ID, m.TS, m.FromMe, m.Sender, m.Who, m.Text, m.Kind, m.Media, m.File, m.State)
	if err != nil {
		return err
	}
	_, err = s.db.Exec(`INSERT INTO rim_chat (jid, is_group, last_ts) VALUES (?, ?, ?)
		ON CONFLICT (jid) DO UPDATE SET last_ts = MAX(last_ts, excluded.last_ts), hidden = 0`, m.Chat, isGroup, m.TS)
	return err
}

func (s *Store) SetText(chat, id, text string) {
	_, _ = s.db.Exec(`UPDATE rim_msg SET text = ? WHERE chat = ? AND id = ?`, text, chat, id)
}

func (s *Store) SetState(chat, id, state string) {
	_, _ = s.db.Exec(`UPDATE rim_msg SET state = ? WHERE chat = ? AND id = ?`, state, chat, id)
}

func (s *Store) SetFile(chat, id, file string) {
	_, _ = s.db.Exec(`UPDATE rim_msg SET file = ? WHERE chat = ? AND id = ?`, file, chat, id)
}

// ReplaceID turns a pending line into the sent message.
func (s *Store) ReplaceID(chat, old, id string, ts int64) {
	_, _ = s.db.Exec(`UPDATE rim_msg SET id = ?, ts = ?, state = 'sent' WHERE chat = ? AND id = ?`, id, ts, chat, old)
}

func (s *Store) SetName(jid, name string, isGroup bool) {
	if name == "" {
		return
	}
	_, _ = s.db.Exec(`INSERT INTO rim_chat (jid, name, is_group) VALUES (?, ?, ?)
		ON CONFLICT (jid) DO UPDATE SET name = excluded.name`, jid, name, isGroup)
}

func (s *Store) Hide(jid string) {
	_, _ = s.db.Exec(`UPDATE rim_chat SET hidden = 1 WHERE jid = ?`, jid)
}

func (s *Store) Get(chat, id string) (Msg, bool) {
	var m Msg
	err := s.db.QueryRow(`SELECT chat, id, ts, from_me, sender, who, text, kind, media, file, state FROM rim_msg WHERE chat = ? AND id = ?`, chat, id).
		Scan(&m.Chat, &m.ID, &m.TS, &m.FromMe, &m.Sender, &m.Who, &m.Text, &m.Kind, &m.Media, &m.File, &m.State)
	return m, err == nil
}

// Last returns the newest n messages of a chat, oldest first.
func (s *Store) Last(chat string, n int) []Msg {
	rows, err := s.db.Query(`SELECT chat, id, ts, from_me, sender, who, text, kind, file, state FROM
		(SELECT * FROM rim_msg WHERE chat = ? ORDER BY ts DESC LIMIT ?) ORDER BY ts`, chat, n)
	if err != nil {
		return nil
	}
	defer rows.Close()
	var out []Msg
	for rows.Next() {
		var m Msg
		if rows.Scan(&m.Chat, &m.ID, &m.TS, &m.FromMe, &m.Sender, &m.Who, &m.Text, &m.Kind, &m.File, &m.State) == nil {
			out = append(out, m)
		}
	}
	return out
}

// Chats returns the chats with something in the last `days` days, at most `n`, newest first.
func (s *Store) Chats(since int64, n int) []Chat {
	rows, err := s.db.Query(`SELECT c.jid, c.name, c.is_group, c.last_ts,
		(SELECT COUNT(*) FROM rim_msg m WHERE m.chat = c.jid AND m.from_me = 0 AND m.ts > c.read_ts)
		FROM rim_chat c WHERE c.hidden = 0 AND c.last_ts >= ? ORDER BY c.last_ts DESC LIMIT ?`, since, n)
	if err != nil {
		return nil
	}
	defer rows.Close()
	var out []Chat
	for rows.Next() {
		var c Chat
		if rows.Scan(&c.JID, &c.Name, &c.IsGroup, &c.LastTS, &c.Unread) == nil {
			out = append(out, c)
		}
	}
	return out
}

// Unread returns the unread incoming messages of a chat (id, sender).
func (s *Store) Unread(chat string) [][2]string {
	rows, err := s.db.Query(`SELECT m.id, m.sender FROM rim_msg m JOIN rim_chat c ON c.jid = m.chat
		WHERE m.chat = ? AND m.from_me = 0 AND m.ts > c.read_ts`, chat)
	if err != nil {
		return nil
	}
	defer rows.Close()
	var out [][2]string
	for rows.Next() {
		var id, sender string
		if rows.Scan(&id, &sender) == nil {
			out = append(out, [2]string{id, sender})
		}
	}
	return out
}

func (s *Store) MarkRead(chat string, ts int64) {
	_, _ = s.db.Exec(`UPDATE rim_chat SET read_ts = MAX(read_ts, ?) WHERE jid = ?`, ts, chat)
}
