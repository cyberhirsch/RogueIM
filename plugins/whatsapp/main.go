// WhatsApp plugin (PRD HB-5): your WhatsApp chats in RogueIM's buddy list,
// through whatsmeow, linked like WhatsApp Web. Unofficial and against
// WhatsApp's terms, so nothing connects before the user has accepted the
// warning. Level C: WhatsApp encrypts end to end, this computer decrypts.
package main

import (
	"context"
	"encoding/json"
	"fmt"
	"mime"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"sort"
	"strings"
	"sync"
	"time"

	"go.mau.fi/whatsmeow"
	"go.mau.fi/whatsmeow/proto/waE2E"
	"go.mau.fi/whatsmeow/store"
	"go.mau.fi/whatsmeow/store/sqlstore"
	"go.mau.fi/whatsmeow/types"
	"go.mau.fi/whatsmeow/types/events"
	waLog "go.mau.fi/whatsmeow/util/log"
	"google.golang.org/protobuf/proto"
	_ "modernc.org/sqlite"
)

const (
	warning = "WhatsApp encrypts end to end, but an unofficial client (whatsmeow) decrypts it on this computer. WhatsApp's servers see who you write to and when."
	// Chats listed: active in the last 60 days, at most 150.
	listDays  = 60
	listMax   = 150
	chatLines = 60
	// Pictures up to this size are fetched as they arrive.
	autoImage = 8 << 20
)

type config struct {
	Accepted bool `json:"accepted"`
}

type Plugin struct {
	mu       sync.Mutex
	host     *Host
	dir      string
	cfg      config
	db       *memDB
	st       *Store
	devices  *sqlstore.Container
	cli      *whatsmeow.Client
	qrCode   string
	pairCode string
	status   string
	open     map[string]bool
	groups   map[string]string
	pending  int
}

func main() {
	host, err := connectHost("whatsapp")
	if err != nil {
		fmt.Fprintln(os.Stderr, "rim-plugin-whatsapp is started by RogueIM (settings > plugins).")
		os.Exit(2)
	}
	dir := os.Getenv("RIM_PLUGIN_DATA")
	if dir == "" {
		dir = "."
	}
	_ = os.MkdirAll(filepath.Join(dir, "media"), 0o700)
	clearOpenCopies()
	p := &Plugin{host: host, dir: dir, open: map[string]bool{}, groups: map[string]string{}}
	if b, err := readSealed(filepath.Join(dir, "whatsapp.json")); err == nil {
		_ = json.Unmarshal(b, &p.cfg)
	}
	host.Send(map[string]any{"t": "channel", "label": "whatsapp", "level": "C", "warning": warning, "files": true})
	if p.cfg.Accepted {
		p.start()
	}
	p.view()
	for m := range host.In {
		switch m.T {
		case "stop":
			p.mu.Lock()
			if p.cli != nil {
				p.cli.Disconnect()
			}
			p.mu.Unlock()
			if p.db != nil {
				_ = p.db.Close()
			}
			clearOpenCopies()
			return
		case "click":
			p.click(m.ID)
		case "input":
			if m.ID == "phone" {
				go p.pairPhone(m.Value)
			}
		case "open":
			p.mu.Lock()
			p.open[m.Chat] = true
			p.mu.Unlock()
			go p.openChat(m.Chat)
		case "close":
			p.mu.Lock()
			delete(p.open, m.Chat)
			p.mu.Unlock()
		case "send":
			go p.sendText(m.Chat, m.Text)
		case "send_file":
			go p.sendFile(m.Chat, m.Path)
		case "action":
			go p.action(m.Chat, m.Line, m.ID)
		case "remove":
			if p.st != nil {
				p.st.Hide(m.Chat)
				p.contacts()
			}
		}
	}
}

func (p *Plugin) saveConfig() {
	b, _ := json.Marshal(p.cfg)
	_ = writeSealed(filepath.Join(p.dir, "whatsapp.json"), b)
}

func (p *Plugin) setStatus(s string) {
	p.mu.Lock()
	p.status = s
	p.mu.Unlock()
	p.view()
}

func (p *Plugin) click(id string) {
	switch id {
	case "accept":
		p.cfg.Accepted = true
		p.saveConfig()
		p.start()
	case "decline", "stop":
		p.mu.Lock()
		if p.cli != nil && p.cli.Store.ID == nil {
			p.cli.Disconnect()
			p.cli = nil
		}
		p.qrCode, p.pairCode = "", ""
		p.mu.Unlock()
		p.cfg.Accepted = false
		p.saveConfig()
	case "relink":
		p.start()
	case "signout":
		p.mu.Lock()
		cli := p.cli
		p.mu.Unlock()
		if cli != nil {
			// Unlinks this computer on the phone too.
			_ = cli.Logout(context.Background())
			cli.Disconnect()
		}
		p.mu.Lock()
		p.cli = nil
		p.mu.Unlock()
		p.cfg.Accepted = false
		p.saveConfig()
		p.status = "signed out: unlinked from your phone"
		p.host.Send(map[string]any{"t": "contacts", "list": []NetContact{}})
	}
	p.view()
}

// start opens the database and connects; without a linked device it shows a QR code.
func (p *Plugin) start() {
	ctx := context.Background()
	if p.db == nil {
		db, err := openMemDB(filepath.Join(p.dir, "whatsapp.db"))
		if err != nil {
			p.setStatus("database: " + err.Error())
			return
		}
		p.db = db
		p.devices = sqlstore.NewWithDB(db.DB, "sqlite3", waLog.Noop)
		if err := p.devices.Upgrade(ctx); err != nil {
			p.setStatus("database: " + err.Error())
			return
		}
		if p.st, err = openStore(db.DB); err != nil {
			p.setStatus("database: " + err.Error())
			return
		}
	}
	device, err := p.devices.GetFirstDevice(ctx)
	if err != nil {
		p.setStatus("database: " + err.Error())
		return
	}
	// The name the phone shows under "Linked devices".
	store.SetOSInfo("RogueIM", [3]uint32{0, 1, 8})
	cli := whatsmeow.NewClient(device, waLog.Noop)
	cli.AddEventHandler(p.event)
	p.mu.Lock()
	if p.cli != nil {
		p.cli.Disconnect()
	}
	p.cli = cli
	p.mu.Unlock()
	p.host.Send(map[string]any{"t": "network", "host": "web.whatsapp.com", "purpose": "WhatsApp (unofficial client)"})
	if cli.Store.ID == nil {
		qrs, err := cli.GetQRChannel(ctx)
		if err != nil {
			p.setStatus(err.Error())
			return
		}
		go func() {
			for item := range qrs {
				p.mu.Lock()
				switch item.Event {
				case "code":
					p.qrCode = item.Code
				case "success":
					p.qrCode, p.pairCode, p.status = "", "", "linked; loading your chats…"
				case "timeout":
					p.qrCode, p.status = "", "the code expired"
				default:
					p.qrCode = ""
					if item.Error != nil {
						p.status = "linking failed: " + item.Error.Error()
					}
				}
				p.mu.Unlock()
				p.view()
			}
		}()
	}
	p.setStatus("connecting…")
	if err := cli.Connect(); err != nil {
		p.setStatus("could not connect: " + err.Error())
		return
	}
	p.contacts()
}

func (p *Plugin) pairPhone(number string) {
	digits := strings.Map(func(r rune) rune {
		if r >= '0' && r <= '9' {
			return r
		}
		return -1
	}, number)
	p.mu.Lock()
	cli := p.cli
	p.mu.Unlock()
	if cli == nil || len(digits) < 8 {
		p.setStatus("enter your number with country code, e.g. +49 170 1234567")
		return
	}
	code, err := cli.PairPhone(context.Background(), digits, true, whatsmeow.PairClientChrome, "Chrome (RogueIM)")
	if err != nil {
		p.setStatus("pairing code: " + err.Error())
		return
	}
	p.mu.Lock()
	p.pairCode = code
	p.mu.Unlock()
	p.view()
}

func (p *Plugin) view() {
	p.mu.Lock()
	defer p.mu.Unlock()
	var items []Item
	switch {
	case !p.cfg.Accepted:
		items = append(items,
			text("Before you link WhatsApp", 0),
			dim("WhatsApp offers no official way in for other programs. RogueIM links like WhatsApp Web, through the unofficial library whatsmeow.", 1),
			dim("That breaks WhatsApp's terms: WhatsApp can block your number, for a while or for good. It is rare, but nobody can rule it out.", 2),
			dim("You can unlink at any time, here or on your phone. Nothing connects until you accept.", 3),
			button("accept", "I accept the risk", 4))
		if p.status != "" {
			items = append(items, dim(p.status, 5))
		}
	case p.cli != nil && p.cli.Store.ID != nil:
		me := p.cli.Store.ID.User
		items = append(items, text("+"+me, 0))
		state := "offline"
		if p.cli.IsConnected() {
			state = "connected"
		}
		n, unread := 0, 0
		if p.st != nil {
			for _, c := range p.st.Chats(time.Now().AddDate(0, 0, -listDays).Unix(), listMax) {
				n++
				unread += c.Unread
			}
		}
		items = append(items, dim(fmt.Sprintf("%d chats · %d unread · %s", n, unread, state), 1),
			button("signout", "unlink", 2))
		if p.status != "" {
			items = append(items, dim(p.status, 3))
		}
	default:
		if p.qrCode != "" {
			items = append(items, qr(p.qrCode, 0),
				dim("On your phone: WhatsApp → Settings → Linked devices → Link a device, then scan.", 1))
		}
		if p.pairCode != "" {
			items = append(items, text("code for your phone: "+p.pairCode, 2),
				dim("Linked devices → Link with phone number instead, then type the code.", 3))
		} else {
			items = append(items, input("phone", "or your number, for a code instead", 2))
		}
		items = append(items, button("stop", "cancel", 4))
		if p.qrCode == "" && p.pairCode == "" {
			items = append(items, button("relink", "new code", 4))
		}
		if p.status != "" {
			items = append(items, dim(p.status, 5))
		}
	}
	items = append(items, dim("Unofficial client · WhatsApp sees who you write to and when.", 9))
	p.host.View(items)
}

// --------------------------------------------------------------- events

func (p *Plugin) event(evt any) {
	switch e := evt.(type) {
	case *events.Connected:
		p.setStatus("")
		go p.loadGroups()
	case *events.Disconnected:
		p.setStatus("disconnected, reconnecting…")
	case *events.PairSuccess:
		p.setStatus("linked; loading your chats…")
	case *events.LoggedOut:
		p.mu.Lock()
		p.cfg.Accepted = false
		p.status = "unlinked on your phone"
		p.mu.Unlock()
		p.saveConfig()
		p.host.Send(map[string]any{"t": "contacts", "list": []NetContact{}})
		p.view()
	case *events.StreamReplaced:
		p.setStatus("WhatsApp was opened elsewhere on this computer")
	case *events.Message:
		p.message(e, true)
	case *events.HistorySync:
		for _, conv := range e.Data.GetConversations() {
			chat, err := types.ParseJID(conv.GetID())
			if err != nil {
				continue
			}
			name := conv.GetName()
			if name == "" {
				name = conv.GetDisplayName()
			}
			if p.st != nil {
				p.st.SetName(p.chatID(chat), name, chat.Server == types.GroupServer)
			}
			var incoming []int64
			for _, hm := range conv.GetMessages() {
				if m, err := p.cli.ParseWebMessage(chat, hm.GetMessage()); err == nil {
					p.message(m, false)
					if !m.Info.IsFromMe {
						incoming = append(incoming, m.Info.Timestamp.Unix())
					}
				}
			}
			// Unread as on the phone: all but the newest UnreadCount incoming messages are read.
			if p.st != nil && len(incoming) > 0 {
				sort.Slice(incoming, func(i, j int) bool { return incoming[i] < incoming[j] })
				read := len(incoming) - int(conv.GetUnreadCount())
				if read > 0 {
					p.st.MarkRead(p.chatID(chat), incoming[read-1])
				}
			}
		}
		p.contacts()
		p.view()
	case *events.PushName:
		if p.st != nil && e.NewPushName != "" {
			p.contacts()
		}
	}
}

func (p *Plugin) loadGroups() {
	p.mu.Lock()
	cli := p.cli
	p.mu.Unlock()
	if cli == nil {
		return
	}
	groups, err := cli.GetJoinedGroups(context.Background())
	if err != nil {
		return
	}
	p.mu.Lock()
	for _, g := range groups {
		p.groups[g.JID.String()] = g.Name
	}
	p.mu.Unlock()
	for _, g := range groups {
		p.st.SetName(g.JID.String(), g.Name, true)
	}
	p.contacts()
	p.view()
}

// chatID is the stable key of a chat: one per person, by phone number where known.
func (p *Plugin) chatID(j types.JID) string {
	j = j.ToNonAD()
	if j.Server == types.HiddenUserServer && p.cli != nil {
		if pn, err := p.cli.Store.LIDs.GetPNForLID(context.Background(), j); err == nil && !pn.IsEmpty() {
			return pn.ToNonAD().String()
		}
	}
	return j.String()
}

// name of a person: as saved on the phone, else their own name, else the number.
func (p *Plugin) name(j types.JID, push string) string {
	j = j.ToNonAD()
	ctx := context.Background()
	if p.cli != nil {
		if j.Server == types.HiddenUserServer {
			if pn, err := p.cli.Store.LIDs.GetPNForLID(ctx, j); err == nil && !pn.IsEmpty() {
				j = pn
			}
		}
		if c, err := p.cli.Store.Contacts.GetContact(ctx, j); err == nil && c.Found {
			for _, n := range []string{c.FullName, c.FirstName, c.BusinessName, c.PushName} {
				if n != "" {
					return n
				}
			}
		}
	}
	if push != "" {
		return push
	}
	if j.Server == types.DefaultUserServer {
		return "+" + j.User
	}
	return j.User
}

func (p *Plugin) message(e *events.Message, live bool) {
	if p.st == nil || e.Info.Chat.Server == types.BroadcastServer || e.Info.Chat.Server == types.NewsletterServer {
		return
	}
	chat := p.chatID(e.Info.Chat)
	isGroup := e.Info.IsGroup
	msg := e.Message
	if pm := msg.GetProtocolMessage(); pm != nil {
		switch pm.GetType() {
		case waE2E.ProtocolMessage_REVOKE:
			p.st.SetText(chat, pm.GetKey().GetID(), "[deleted]")
		case waE2E.ProtocolMessage_MESSAGE_EDIT:
			if t := plainText(pm.GetEditedMessage()); t != "" {
				p.st.SetText(chat, pm.GetKey().GetID(), t)
			}
		}
		p.refresh(chat)
		return
	}
	who := "me"
	if !e.Info.IsFromMe {
		who = p.name(e.Info.Sender, e.Info.PushName)
	}
	text, kind := describe(msg)
	if text == "" {
		return
	}
	var media []byte
	if kind != "" && kind != "location" && kind != "sticker" && kind != "contact" {
		media, _ = proto.Marshal(msg)
	}
	m := Msg{Chat: chat, ID: e.Info.ID, TS: e.Info.Timestamp.Unix(), FromMe: e.Info.IsFromMe, Sender: e.Info.Sender.ToNonAD().String(), Who: who, Text: text, Kind: kind, Media: media}
	if e.Info.IsFromMe {
		m.State = "sent"
	}
	if err := p.st.Put(m, isGroup); err != nil {
		return
	}
	if !isGroup && !e.Info.IsFromMe {
		p.st.SetName(chat, p.name(e.Info.Sender, e.Info.PushName), false)
	}
	if kind == "image" && msg.GetImageMessage().GetFileLength() < autoImage {
		if live {
			go p.fetch(chat, e.Info.ID)
		}
	}
	if live {
		p.mu.Lock()
		open := p.open[chat]
		p.mu.Unlock()
		if open && !e.Info.IsFromMe {
			p.markRead(chat)
		}
		p.contacts()
		p.refresh(chat)
		if !e.Info.IsFromMe {
			name := who
			if isGroup {
				name = who + " in " + p.chatName(chat, true)
			}
			p.host.Send(map[string]any{"t": "incoming", "chat": chat, "name": name})
		}
		p.view()
	}
}

// plainText is the text of a message, if it is one.
func plainText(m *waE2E.Message) string {
	if t := m.GetConversation(); t != "" {
		return t
	}
	return m.GetExtendedTextMessage().GetText()
}

func dur(s uint32) string { return fmt.Sprintf("%d:%02d", s/60, s%60) }

// describe turns a message into a chat line: what RogueIM can't show becomes a
// line with a link (PRD HB-8).
func describe(m *waE2E.Message) (string, string) {
	caption := func(c string) string {
		if c == "" {
			return ""
		}
		return "\n" + c
	}
	switch {
	case plainText(m) != "":
		return plainText(m), ""
	case m.GetImageMessage() != nil:
		return strings.TrimPrefix(caption(m.GetImageMessage().GetCaption()), "\n"), "image"
	case m.GetVideoMessage() != nil:
		return "[video " + dur(m.GetVideoMessage().GetSeconds()) + "]" + caption(m.GetVideoMessage().GetCaption()), "video"
	case m.GetAudioMessage() != nil:
		if m.GetAudioMessage().GetPTT() {
			return "[voice message " + dur(m.GetAudioMessage().GetSeconds()) + "]", "audio"
		}
		return "[audio " + dur(m.GetAudioMessage().GetSeconds()) + "]", "audio"
	case m.GetDocumentMessage() != nil:
		return "[file: " + m.GetDocumentMessage().GetFileName() + "]" + caption(m.GetDocumentMessage().GetCaption()), "doc"
	case m.GetStickerMessage() != nil:
		return "[sticker]", "sticker"
	case m.GetLocationMessage() != nil:
		l := m.GetLocationMessage()
		return fmt.Sprintf("[location %.5f, %.5f]", l.GetDegreesLatitude(), l.GetDegreesLongitude()), "location"
	case m.GetLiveLocationMessage() != nil:
		return "[live location]", "sticker"
	case m.GetContactMessage() != nil:
		return "[contact: " + m.GetContactMessage().GetDisplayName() + "]", "contact"
	case m.GetPollCreationMessage() != nil:
		return "[poll: " + m.GetPollCreationMessage().GetName() + "]", "sticker"
	}
	return "", ""
}

func (p *Plugin) chatName(chat string, isGroup bool) string {
	for _, c := range p.st.Chats(0, 100000) {
		if c.JID == chat && c.Name != "" {
			return c.Name
		}
	}
	if j, err := types.ParseJID(chat); err == nil && !isGroup {
		return p.name(j, "")
	}
	return chat
}

// contacts sends the chat list: recent chats, by name.
func (p *Plugin) contacts() {
	if p.st == nil {
		return
	}
	chats := p.st.Chats(time.Now().AddDate(0, 0, -listDays).Unix(), listMax)
	list := make([]NetContact, 0, len(chats))
	for _, c := range chats {
		name := c.Name
		info := ""
		if j, err := types.ParseJID(c.JID); err == nil {
			if c.IsGroup || j.Server == types.GroupServer {
				info = "group"
				if name == "" {
					name = "group"
				}
			} else {
				if n := p.name(j, ""); n != "" && !strings.HasPrefix(n, "+") || name == "" {
					name = n
				}
				info = "+" + j.User
			}
		}
		list = append(list, NetContact{ID: c.JID, Name: name, Info: info, Unread: uint32(c.Unread)})
	}
	sort.Slice(list, func(i, j int) bool { return strings.ToLower(list[i].Name) < strings.ToLower(list[j].Name) })
	p.host.Send(map[string]any{"t": "contacts", "list": list})
}

// refresh sends the lines of a chat if it is open.
func (p *Plugin) refresh(chat string) {
	p.mu.Lock()
	open := p.open[chat]
	p.mu.Unlock()
	if !open {
		return
	}
	var lines []NetLine
	for _, m := range p.st.Last(chat, chatLines) {
		l := NetLine{ID: m.ID, TS: m.TS, FromMe: m.FromMe, Who: m.Who, Text: m.Text, State: m.State}
		if m.FromMe {
			l.Who = "me"
		}
		switch m.Kind {
		case "image":
			if m.File != "" {
				l.Image = m.File
			} else {
				l.Actions = []NetAction{{ID: "fetch", Label: "load picture"}}
				if l.Text == "" {
					l.Text = "[picture]"
				}
			}
		case "audio":
			l.Actions = []NetAction{{ID: "open", Label: "play"}}
		case "video", "doc":
			l.Actions = []NetAction{{ID: "open", Label: "open"}, {ID: "save", Label: "save"}}
		case "location":
			l.Actions = []NetAction{{ID: "map", Label: "map"}}
		}
		lines = append(lines, l)
	}
	if lines == nil {
		lines = []NetLine{}
	}
	p.host.Send(map[string]any{"t": "lines", "chat": chat, "lines": lines})
}

func (p *Plugin) openChat(chat string) {
	if p.st == nil {
		return
	}
	p.markRead(chat)
	p.refresh(chat)
	p.contacts()
	p.view()
}

// markRead marks a chat read here and on WhatsApp (blue ticks, as WhatsApp Web does).
func (p *Plugin) markRead(chat string) {
	unread := p.st.Unread(chat)
	p.st.MarkRead(chat, time.Now().Unix())
	p.mu.Lock()
	cli := p.cli
	p.mu.Unlock()
	if cli == nil || len(unread) == 0 {
		return
	}
	cj, err := types.ParseJID(chat)
	if err != nil {
		return
	}
	bySender := map[string][]types.MessageID{}
	for _, u := range unread {
		bySender[u[1]] = append(bySender[u[1]], u[0])
	}
	for sender, ids := range bySender {
		sj, _ := types.ParseJID(sender)
		_ = cli.MarkRead(context.Background(), ids, time.Now(), cj, sj)
	}
}

// --------------------------------------------------------------- sending

func (p *Plugin) sendText(chat, body string) {
	p.send(chat, &waE2E.Message{Conversation: proto.String(body)}, Msg{Text: body})
}

func (p *Plugin) send(chat string, msg *waE2E.Message, line Msg) {
	p.mu.Lock()
	cli := p.cli
	p.pending++
	pid := fmt.Sprintf("p:%d", p.pending)
	p.mu.Unlock()
	to, err := types.ParseJID(chat)
	if cli == nil || p.st == nil || err != nil {
		return
	}
	line.Chat, line.ID, line.TS, line.FromMe, line.Who, line.State = chat, pid, time.Now().Unix(), true, "me", "sending"
	_ = p.st.Put(line, to.Server == types.GroupServer)
	p.refresh(chat)
	resp, err := cli.SendMessage(context.Background(), to, msg)
	if err != nil {
		p.st.SetState(chat, pid, "failed")
		p.st.SetText(chat, pid, line.Text+"\n[not sent: "+err.Error()+"]")
	} else {
		p.st.ReplaceID(chat, pid, resp.ID, resp.Timestamp.Unix())
	}
	p.refresh(chat)
	p.contacts()
}

func (p *Plugin) sendFile(chat, path string) {
	p.mu.Lock()
	cli := p.cli
	p.mu.Unlock()
	data, err := os.ReadFile(path)
	if cli == nil || err != nil {
		return
	}
	ext := strings.ToLower(filepath.Ext(path))
	mt := mime.TypeByExtension(ext)
	if mt == "" {
		mt = "application/octet-stream"
	}
	ctx := context.Background()
	isImage := ext == ".jpg" || ext == ".jpeg" || ext == ".png"
	kind := whatsmeow.MediaDocument
	if isImage {
		kind = whatsmeow.MediaImage
	}
	up, err := cli.Upload(ctx, data, kind)
	if err != nil {
		p.setStatus("could not send " + filepath.Base(path) + ": " + err.Error())
		return
	}
	// A copy, so the chat can show it later.
	copyPath := filepath.Join(p.dir, "media", fmt.Sprintf("out-%d%s", time.Now().UnixNano(), ext))
	_ = writeSealed(copyPath, data)
	var msg *waE2E.Message
	line := Msg{File: copyPath}
	if isImage {
		msg = &waE2E.Message{ImageMessage: &waE2E.ImageMessage{
			URL: proto.String(up.URL), DirectPath: proto.String(up.DirectPath), MediaKey: up.MediaKey,
			Mimetype: proto.String(mt), FileEncSHA256: up.FileEncSHA256, FileSHA256: up.FileSHA256, FileLength: proto.Uint64(up.FileLength),
		}}
		line.Kind = "image"
	} else {
		name := filepath.Base(path)
		msg = &waE2E.Message{DocumentMessage: &waE2E.DocumentMessage{
			URL: proto.String(up.URL), DirectPath: proto.String(up.DirectPath), MediaKey: up.MediaKey, FileName: proto.String(name),
			Mimetype: proto.String(mt), FileEncSHA256: up.FileEncSHA256, FileSHA256: up.FileSHA256, FileLength: proto.Uint64(up.FileLength),
		}}
		line.Kind, line.Text = "doc", "[file: "+name+"]"
	}
	line.Media, _ = proto.Marshal(msg)
	p.send(chat, msg, line)
}

// --------------------------------------------------------------- media

// fetch downloads a message's media into the plugin folder.
func (p *Plugin) fetch(chat, id string) (string, error) {
	m, ok := p.st.Get(chat, id)
	if !ok {
		return "", fmt.Errorf("message not found")
	}
	if m.File != "" {
		if _, err := os.Stat(m.File); err == nil {
			return m.File, nil
		}
	}
	p.mu.Lock()
	cli := p.cli
	p.mu.Unlock()
	if cli == nil || len(m.Media) == 0 {
		return "", fmt.Errorf("not available")
	}
	var wm waE2E.Message
	if err := proto.Unmarshal(m.Media, &wm); err != nil {
		return "", err
	}
	data, err := cli.DownloadAny(context.Background(), &wm)
	if err != nil {
		return "", err
	}
	ext := ".bin"
	switch {
	case wm.GetImageMessage() != nil:
		ext = extFor(wm.GetImageMessage().GetMimetype(), ".jpg")
	case wm.GetVideoMessage() != nil:
		ext = extFor(wm.GetVideoMessage().GetMimetype(), ".mp4")
	case wm.GetAudioMessage() != nil:
		ext = ".ogg"
	case wm.GetDocumentMessage() != nil:
		ext = filepath.Ext(wm.GetDocumentMessage().GetFileName())
	}
	safe := strings.Map(func(r rune) rune {
		if r == '/' || r == '\\' || r == ':' {
			return '_'
		}
		return r
	}, id)
	path := filepath.Join(p.dir, "media", safe+ext)
	if err := writeSealed(path, data); err != nil {
		return "", err
	}
	p.st.SetFile(chat, id, path)
	p.refresh(chat)
	return path, nil
}

func extFor(mt, def string) string {
	if exts, _ := mime.ExtensionsByType(mt); len(exts) > 0 {
		return exts[len(exts)-1]
	}
	return def
}

func (p *Plugin) action(chat, line, id string) {
	m, ok := p.st.Get(chat, line)
	if !ok {
		return
	}
	switch id {
	case "map":
		var lat, lon float64
		if _, err := fmt.Sscanf(m.Text, "[location %f, %f]", &lat, &lon); err == nil {
			openPath(fmt.Sprintf("https://www.openstreetmap.org/?mlat=%f&mlon=%f#map=16/%f/%f", lat, lon, lat, lon))
		}
	case "fetch", "open", "save":
		path, err := p.fetch(chat, line)
		if err != nil {
			p.setStatus("could not load it: " + err.Error())
			return
		}
		// The cache is sealed: the viewer gets a plain copy in a temp folder that
		// is emptied when the plugin stops; "save" puts one into Downloads.
		name := p.fileName(chat, line)
		if id == "open" {
			if out, err := plainCopy(path, openDir(), name); err == nil {
				openPath(out)
			}
		} else if id == "save" {
			if out, err := plainCopy(path, downloadsDir(), name); err == nil {
				p.setStatus("saved to " + out)
				openPath(filepath.Dir(out))
			}
		}
	}
}

// fileName is a document's own name, or "" to use the cache name.
func (p *Plugin) fileName(chat, line string) string {
	m, ok := p.st.Get(chat, line)
	if !ok || len(m.Media) == 0 {
		return ""
	}
	var wm waE2E.Message
	if proto.Unmarshal(m.Media, &wm) != nil || wm.GetDocumentMessage() == nil {
		return ""
	}
	return filepath.Base(strings.Map(func(r rune) rune {
		if r == '/' || r == '\\' || r == ':' {
			return '_'
		}
		return r
	}, wm.GetDocumentMessage().GetFileName()))
}

// openPath hands a file or link to the system.
func openPath(path string) {
	switch runtime.GOOS {
	case "windows":
		_ = exec.Command("rundll32", "url.dll,FileProtocolHandler", path).Start()
	case "darwin":
		_ = exec.Command("open", path).Start()
	default:
		_ = exec.Command("xdg-open", path).Start()
	}
}
