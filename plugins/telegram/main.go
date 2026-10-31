// Telegram plugin (PRD HB-5): your Telegram chats in RogueIM's buddy list,
// through Telegram's own client API (MTProto, gotd/td) with the user's own
// api_id. Level D: Telegram's cloud chats are not end-to-end encrypted.
// Telegram keeps the history, so the plugin keeps no messages of its own:
// the chat list and the lines are fetched from Telegram as needed.
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"mime"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"

	"github.com/gotd/td/session"
	"github.com/gotd/td/telegram"
	"github.com/gotd/td/telegram/auth/qrlogin"
	"github.com/gotd/td/telegram/downloader"
	"github.com/gotd/td/telegram/updates"
	"github.com/gotd/td/telegram/uploader"
	"github.com/gotd/td/tg"
	"github.com/gotd/td/tgerr"
)

const (
	warning = "Telegram chats are not end-to-end encrypted: Telegram's servers can read them. (Secret chats exist only in Telegram's phone apps.)"
	// Chats listed: active in the last 60 days, at most 100 (one request).
	listDays  = 60
	listMax   = 100
	chatLines = 60
	// Pictures up to this size are fetched as they are shown.
	autoImage = 8 << 20
	version   = "0.1.13"
)

type config struct {
	APIID   int    `json:"api_id"`
	APIHash string `json:"api_hash"`
}

// A chat in the buddy list.
type dialog struct {
	id     string
	name   string
	info   string
	unread int
	date   int
}

type Plugin struct {
	mu       sync.Mutex
	host     *Host
	dir      string
	cfg      config
	cancel   context.CancelFunc
	api      *tg.Client
	gaps     *updates.Manager
	self     *tg.User
	qrURL    string
	needPass bool
	pass     chan string
	status   string
	online   bool
	users    map[int64]*tg.User
	chats    map[int64]*tg.Chat
	channels map[int64]*tg.Channel
	dialogs  map[string]*dialog
	open     map[string]bool
	hist     map[string][]*tg.Message
	pending  map[string][]NetLine
	loading  map[string]bool
}

func main() {
	host, err := connectHost("telegram")
	if err != nil {
		fmt.Fprintln(os.Stderr, "rim-plugin-telegram is started by RogueIM (settings > plugins).")
		os.Exit(2)
	}
	dir := os.Getenv("RIM_PLUGIN_DATA")
	if dir == "" {
		dir = "."
	}
	_ = os.MkdirAll(filepath.Join(dir, "media"), 0o700)
	clearOpenCopies()
	p := &Plugin{host: host, dir: dir, users: map[int64]*tg.User{}, chats: map[int64]*tg.Chat{}, channels: map[int64]*tg.Channel{},
		dialogs: map[string]*dialog{}, open: map[string]bool{}, hist: map[string][]*tg.Message{}, pending: map[string][]NetLine{}, loading: map[string]bool{}}
	if b, err := readSealed(filepath.Join(dir, "telegram.json")); err == nil {
		_ = json.Unmarshal(b, &p.cfg)
	}
	host.Send(map[string]any{"t": "channel", "label": "telegram", "level": "D", "warning": warning, "files": true})
	if p.cfg.APIID != 0 && p.cfg.APIHash != "" {
		p.start()
	}
	p.view()
	for m := range host.In {
		switch m.T {
		case "stop":
			p.stop()
			clearOpenCopies()
			return
		case "click":
			p.click(m.ID)
		case "input":
			p.input(m.ID, m.Value)
		case "open":
			p.mu.Lock()
			p.open[m.Chat] = true
			p.mu.Unlock()
			go p.openChat(m.Chat)
		case "close":
			p.mu.Lock()
			delete(p.open, m.Chat)
			delete(p.hist, m.Chat)
			p.mu.Unlock()
		case "send":
			go p.sendText(m.Chat, m.Text)
		case "send_file":
			go p.sendFile(m.Chat, m.Path)
		case "action":
			go p.action(m.Chat, m.Line, m.ID)
		case "remove":
			p.mu.Lock()
			delete(p.dialogs, m.Chat)
			p.mu.Unlock()
			p.contacts()
		}
	}
}

func (p *Plugin) saveConfig() {
	b, _ := json.Marshal(p.cfg)
	_ = writeSealed(filepath.Join(p.dir, "telegram.json"), b)
}

func (p *Plugin) setStatus(s string) {
	p.mu.Lock()
	p.status = s
	p.mu.Unlock()
	p.view()
}

func (p *Plugin) input(id, value string) {
	value = strings.TrimSpace(value)
	switch id {
	case "api_id":
		n, err := strconv.Atoi(value)
		if err != nil || n <= 0 {
			p.setStatus("api_id is a number")
			return
		}
		p.cfg.APIID = n
		p.saveConfig()
	case "api_hash":
		p.cfg.APIHash = value
		p.saveConfig()
	case "pass":
		p.mu.Lock()
		ch := p.pass
		p.mu.Unlock()
		if ch != nil && value != "" {
			select {
			case ch <- value:
			default:
			}
		}
		return
	}
	p.mu.Lock()
	running := p.cancel != nil
	p.mu.Unlock()
	if !running && p.cfg.APIID != 0 && p.cfg.APIHash != "" {
		p.start()
	}
	p.view()
}

func (p *Plugin) click(id string) {
	switch id {
	case "relink":
		p.stop()
		p.start()
	case "keys":
		p.stop()
		p.cfg = config{}
		p.saveConfig()
		p.setStatus("")
	case "signout":
		p.mu.Lock()
		api := p.api
		p.mu.Unlock()
		if api != nil {
			ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
			// Ends this session on Telegram too: it leaves "Devices" on the phone.
			_, _ = api.AuthLogOut(ctx)
			cancel()
		}
		p.stop()
		_ = os.Remove(filepath.Join(p.dir, "telegram.session"))
		p.mu.Lock()
		p.dialogs = map[string]*dialog{}
		p.self = nil
		p.status = "signed out"
		p.mu.Unlock()
		p.host.Send(map[string]any{"t": "contacts", "list": []NetContact{}})
	}
	p.view()
}

func (p *Plugin) stop() {
	p.mu.Lock()
	if p.cancel != nil {
		p.cancel()
	}
	p.cancel, p.api, p.online, p.qrURL, p.needPass = nil, nil, false, "", false
	p.mu.Unlock()
}

// start runs the client until stopped: it logs in by QR code if needed,
// then follows Telegram's updates. It reconnects after errors.
func (p *Plugin) start() {
	ctx, cancel := context.WithCancel(context.Background())
	p.mu.Lock()
	p.cancel = cancel
	p.mu.Unlock()
	p.host.Send(map[string]any{"t": "network", "host": "Telegram's data centres", "purpose": "Telegram (official client API)"})
	go func() {
		for wait := 5 * time.Second; ctx.Err() == nil; {
			err := p.run(ctx)
			if ctx.Err() != nil {
				return
			}
			p.mu.Lock()
			p.online = false
			p.mu.Unlock()
			if err != nil {
				p.setStatus("offline: " + err.Error())
			}
			select {
			case <-ctx.Done():
				return
			case <-time.After(wait):
			}
			if wait < 5*time.Minute {
				wait *= 2
			}
		}
	}()
}

func (p *Plugin) run(ctx context.Context) error {
	d := tg.NewUpdateDispatcher()
	gaps := updates.New(updates.Config{Handler: d})
	loggedIn := qrlogin.OnLoginToken(d)
	p.handlers(&d)
	authed := false
	var mu sync.Mutex
	// Before login the login token arrives straight; after it, through the gap manager.
	handler := telegram.UpdateHandlerFunc(func(ctx context.Context, u tg.UpdatesClass) error {
		mu.Lock()
		a := authed
		mu.Unlock()
		if a {
			return gaps.Handle(ctx, u)
		}
		return d.Handle(ctx, u)
	})
	client := telegram.NewClient(p.cfg.APIID, p.cfg.APIHash, telegram.Options{
		SessionStorage: sealedSession{filepath.Join(p.dir, "telegram.session")},
		UpdateHandler:  handler,
		Device:         telegram.DeviceConfig{DeviceModel: "RogueIM", SystemVersion: runtime.GOOS, AppVersion: version},
	})
	return client.Run(ctx, func(ctx context.Context) error {
		api := client.API()
		p.mu.Lock()
		p.api, p.gaps = api, gaps
		p.mu.Unlock()
		st, err := client.Auth().Status(ctx)
		if err != nil {
			return err
		}
		if !st.Authorized {
			p.setStatus("")
			_, err := client.QR().Auth(ctx, loggedIn, func(ctx context.Context, t qrlogin.Token) error {
				p.mu.Lock()
				p.qrURL = t.URL()
				p.mu.Unlock()
				p.view()
				return nil
			})
			p.mu.Lock()
			p.qrURL = ""
			p.mu.Unlock()
			if tgerr.Is(err, "SESSION_PASSWORD_NEEDED") {
				if err := p.password(ctx, client); err != nil {
					return err
				}
			} else if err != nil {
				if tgerr.Is(err, "API_ID_INVALID") {
					p.stop()
					p.setStatus("Telegram does not know this api_id and api_hash")
					return nil
				}
				return err
			}
		}
		self, err := client.Self(ctx)
		if err != nil {
			return err
		}
		mu.Lock()
		authed = true
		mu.Unlock()
		p.mu.Lock()
		p.self, p.online, p.status = self, true, ""
		p.users[self.ID] = self
		p.mu.Unlock()
		p.view()
		return gaps.Run(ctx, api, self.ID, updates.AuthOptions{OnStart: func(ctx context.Context) {
			go p.loadDialogs(ctx)
		}})
	})
}

// password asks for the cloud password (two-step verification) until it fits.
func (p *Plugin) password(ctx context.Context, client *telegram.Client) error {
	ch := make(chan string, 1)
	p.mu.Lock()
	p.needPass, p.pass = true, ch
	p.mu.Unlock()
	p.view()
	defer func() {
		p.mu.Lock()
		p.needPass, p.pass = false, nil
		p.mu.Unlock()
	}()
	for {
		select {
		case <-ctx.Done():
			return ctx.Err()
		case pw := <-ch:
			_, err := client.Auth().Password(ctx, pw)
			if err == nil {
				return nil
			}
			if tgerr.Is(err, "PASSWORD_HASH_INVALID") {
				p.setStatus("wrong password")
				continue
			}
			return err
		}
	}
}

func (p *Plugin) view() {
	p.mu.Lock()
	defer p.mu.Unlock()
	var items []Item
	switch {
	case p.cfg.APIID == 0 || p.cfg.APIHash == "":
		items = append(items,
			text("Your own Telegram API key", 0),
			dim("Telegram lets programs in with a key of their own. Get one at my.telegram.org → API development tools (log in with your number; any app name will do).", 1),
			Item{Kind: "input", ID: "api_id", Text: idText(p.cfg.APIID), Hint: "api_id (a number)", Row: 2},
			Item{Kind: "input", ID: "api_hash", Text: p.cfg.APIHash, Hint: "api_hash", Row: 3})
	case p.self != nil:
		me := strings.TrimSpace(p.self.FirstName + " " + p.self.LastName)
		if p.self.Phone != "" {
			me += " (+" + p.self.Phone + ")"
		}
		items = append(items, text(me, 0))
		state := "offline"
		if p.online {
			state = "connected"
		}
		unread := 0
		for _, d := range p.dialogs {
			unread += d.unread
		}
		items = append(items, dim(fmt.Sprintf("%d chats · %d unread · %s", len(p.dialogs), unread, state), 1),
			button("signout", "sign out", 2))
	case p.needPass:
		items = append(items, text("Two-step verification", 0),
			Item{Kind: "input", ID: "pass", Hint: "your Telegram cloud password", Row: 1})
	case p.qrURL != "":
		items = append(items, qr(p.qrURL, 0),
			dim("On your phone: Telegram → Settings → Devices → Link Desktop Device, then scan.", 1))
	default:
		items = append(items, dim("connecting to Telegram…", 0), button("keys", "other API key", 4))
	}
	if p.self == nil && p.cancel != nil {
		items = append(items, button("relink", "new code", 4))
	}
	if p.status != "" {
		items = append(items, dim(p.status, 5))
	}
	items = append(items, dim("Not end-to-end encrypted · Telegram can read these chats.", 9))
	p.host.View(items)
}

func idText(n int) string {
	if n == 0 {
		return ""
	}
	return strconv.Itoa(n)
}

// --------------------------------------------------------------- peers

func (p *Plugin) remember(users []tg.UserClass, chats []tg.ChatClass) {
	p.mu.Lock()
	defer p.mu.Unlock()
	for _, u := range users {
		if u, ok := u.(*tg.User); ok && !u.Min {
			p.users[u.ID] = u
		}
	}
	for _, c := range chats {
		switch c := c.(type) {
		case *tg.Chat:
			p.chats[c.ID] = c
		case *tg.Channel:
			if !c.Min {
				p.channels[c.ID] = c
			}
		}
	}
}

func (p *Plugin) rememberEntities(e tg.Entities) {
	p.mu.Lock()
	defer p.mu.Unlock()
	for id, u := range e.Users {
		if !u.Min {
			p.users[id] = u
		}
	}
	for id, c := range e.Chats {
		p.chats[id] = c
	}
	for id, c := range e.Channels {
		if !c.Min {
			p.channels[id] = c
		}
	}
}

// chatID of a peer: u<user>, g<basic group>, c<channel or supergroup>.
func chatID(peer tg.PeerClass) string {
	switch pr := peer.(type) {
	case *tg.PeerUser:
		return "u" + strconv.FormatInt(pr.UserID, 10)
	case *tg.PeerChat:
		return "g" + strconv.FormatInt(pr.ChatID, 10)
	case *tg.PeerChannel:
		return "c" + strconv.FormatInt(pr.ChannelID, 10)
	}
	return ""
}

func (p *Plugin) inputPeer(chat string) (tg.InputPeerClass, error) {
	if len(chat) < 2 {
		return nil, errors.New("unknown chat")
	}
	id, err := strconv.ParseInt(chat[1:], 10, 64)
	if err != nil {
		return nil, err
	}
	p.mu.Lock()
	defer p.mu.Unlock()
	switch chat[0] {
	case 'u':
		if p.self != nil && id == p.self.ID {
			return &tg.InputPeerSelf{}, nil
		}
		if u, ok := p.users[id]; ok {
			return &tg.InputPeerUser{UserID: id, AccessHash: u.AccessHash}, nil
		}
	case 'g':
		return &tg.InputPeerChat{ChatID: id}, nil
	case 'c':
		if c, ok := p.channels[id]; ok {
			return &tg.InputPeerChannel{ChannelID: id, AccessHash: c.AccessHash}, nil
		}
	}
	return nil, errors.New("unknown chat")
}

func userName(u *tg.User) string {
	if n := strings.TrimSpace(u.FirstName + " " + u.LastName); n != "" {
		return n
	}
	if u.Username != "" {
		return "@" + u.Username
	}
	if u.Phone != "" {
		return "+" + u.Phone
	}
	return "user " + strconv.FormatInt(u.ID, 10)
}

// describePeer is the name and the info line of a chat; ok is false for
// what does not belong in the buddy list (channels, deleted accounts).
func (p *Plugin) describePeer(chat string) (name, info string, ok bool) {
	if len(chat) < 2 {
		return "", "", false
	}
	id, _ := strconv.ParseInt(chat[1:], 10, 64)
	p.mu.Lock()
	defer p.mu.Unlock()
	switch chat[0] {
	case 'u':
		u, found := p.users[id]
		if !found || u.Deleted {
			return "", "", false
		}
		if u.Self {
			return "Saved Messages", "notes to yourself", true
		}
		switch {
		case u.Bot:
			info = "bot"
		case u.Username != "":
			info = "@" + u.Username
		case u.Phone != "":
			info = "+" + u.Phone
		}
		return userName(u), info, true
	case 'g':
		c, found := p.chats[id]
		if !found || c.Deactivated {
			return "", "", false
		}
		return c.Title, "group", true
	case 'c':
		c, found := p.channels[id]
		if !found || c.Broadcast {
			return "", "", false
		}
		return c.Title, "group", true
	}
	return "", "", false
}

func (p *Plugin) sender(m *tg.Message) string {
	if m.Out {
		return "me"
	}
	from, ok := m.GetFromID()
	if !ok {
		from = m.PeerID
	}
	if u, ok := from.(*tg.PeerUser); ok {
		p.mu.Lock()
		user, found := p.users[u.UserID]
		p.mu.Unlock()
		if found {
			return userName(user)
		}
	}
	name, _, _ := p.describePeer(chatID(from))
	if name == "" {
		name = "?"
	}
	return name
}

// --------------------------------------------------------------- the list

func (p *Plugin) loadDialogs(ctx context.Context) {
	p.mu.Lock()
	api := p.api
	p.mu.Unlock()
	if api == nil {
		return
	}
	res, err := api.MessagesGetDialogs(ctx, &tg.MessagesGetDialogsRequest{OffsetPeer: &tg.InputPeerEmpty{}, Limit: listMax})
	if err != nil {
		p.setStatus("chat list: " + err.Error())
		return
	}
	var dialogs []tg.DialogClass
	var msgs []tg.MessageClass
	switch r := res.(type) {
	case *tg.MessagesDialogs:
		p.remember(r.Users, r.Chats)
		dialogs, msgs = r.Dialogs, r.Messages
	case *tg.MessagesDialogsSlice:
		p.remember(r.Users, r.Chats)
		dialogs, msgs = r.Dialogs, r.Messages
	}
	dates := map[string]int{}
	for _, m := range msgs {
		if m, ok := m.(*tg.Message); ok {
			dates[chatID(m.PeerID)+"/"+strconv.Itoa(m.ID)] = m.Date
		}
	}
	since := int(time.Now().AddDate(0, 0, -listDays).Unix())
	list := map[string]*dialog{}
	for _, dc := range dialogs {
		d, ok := dc.(*tg.Dialog)
		if !ok {
			continue
		}
		id := chatID(d.Peer)
		date := dates[id+"/"+strconv.Itoa(d.TopMessage)]
		name, info, ok := p.describePeer(id)
		if !ok || date < since {
			continue
		}
		list[id] = &dialog{id: id, name: name, info: info, unread: d.UnreadCount, date: date}
	}
	p.mu.Lock()
	p.dialogs = list
	p.mu.Unlock()
	p.contacts()
	p.view()
}

func (p *Plugin) contacts() {
	p.mu.Lock()
	list := make([]NetContact, 0, len(p.dialogs))
	for _, d := range p.dialogs {
		list = append(list, NetContact{ID: d.id, Name: d.name, Info: d.info, Unread: uint32(d.unread)})
	}
	p.mu.Unlock()
	sort.Slice(list, func(i, j int) bool { return strings.ToLower(list[i].Name) < strings.ToLower(list[j].Name) })
	p.host.Send(map[string]any{"t": "contacts", "list": list})
}

// --------------------------------------------------------------- updates

func (p *Plugin) handlers(d *tg.UpdateDispatcher) {
	d.OnNewMessage(func(ctx context.Context, e tg.Entities, u *tg.UpdateNewMessage) error {
		p.rememberEntities(e)
		if m, ok := u.Message.(*tg.Message); ok {
			p.incoming(m)
		}
		return nil
	})
	d.OnNewChannelMessage(func(ctx context.Context, e tg.Entities, u *tg.UpdateNewChannelMessage) error {
		p.rememberEntities(e)
		if m, ok := u.Message.(*tg.Message); ok {
			p.incoming(m)
		}
		return nil
	})
	d.OnEditMessage(func(ctx context.Context, e tg.Entities, u *tg.UpdateEditMessage) error {
		if m, ok := u.Message.(*tg.Message); ok {
			p.edited(m)
		}
		return nil
	})
	d.OnEditChannelMessage(func(ctx context.Context, e tg.Entities, u *tg.UpdateEditChannelMessage) error {
		if m, ok := u.Message.(*tg.Message); ok {
			p.edited(m)
		}
		return nil
	})
	// Read on another device.
	d.OnReadHistoryInbox(func(ctx context.Context, e tg.Entities, u *tg.UpdateReadHistoryInbox) error {
		p.setUnread(chatID(u.Peer), u.StillUnreadCount)
		return nil
	})
	d.OnReadChannelInbox(func(ctx context.Context, e tg.Entities, u *tg.UpdateReadChannelInbox) error {
		p.setUnread("c"+strconv.FormatInt(u.ChannelID, 10), u.StillUnreadCount)
		return nil
	})
}

func (p *Plugin) setUnread(chat string, n int) {
	p.mu.Lock()
	d, ok := p.dialogs[chat]
	changed := ok && d.unread != n
	if changed {
		d.unread = n
	}
	p.mu.Unlock()
	if changed {
		p.contacts()
		p.view()
	}
}

func (p *Plugin) incoming(m *tg.Message) {
	chat := chatID(m.PeerID)
	name, info, ok := p.describePeer(chat)
	if !ok {
		return
	}
	p.mu.Lock()
	d, known := p.dialogs[chat]
	if !known {
		d = &dialog{id: chat}
		p.dialogs[chat] = d
	}
	d.name, d.info, d.date = name, info, m.Date
	open := p.open[chat]
	if !m.Out && !open {
		d.unread++
	}
	if open {
		p.hist[chat] = appendMsg(p.hist[chat], m)
	}
	p.mu.Unlock()
	if open {
		p.refresh(chat)
		if !m.Out {
			p.markRead(chat, m.ID)
		}
	}
	p.contacts()
	if !m.Out {
		who := p.sender(m)
		if chat[0] != 'u' {
			who += " in " + name
		}
		p.host.Send(map[string]any{"t": "incoming", "chat": chat, "name": who})
	}
	p.view()
}

func appendMsg(h []*tg.Message, m *tg.Message) []*tg.Message {
	for i, o := range h {
		if o.ID == m.ID {
			h[i] = m
			return h
		}
	}
	h = append(h, m)
	if len(h) > chatLines {
		h = h[len(h)-chatLines:]
	}
	return h
}

func (p *Plugin) edited(m *tg.Message) {
	chat := chatID(m.PeerID)
	p.mu.Lock()
	open := p.open[chat]
	if open {
		for i, o := range p.hist[chat] {
			if o.ID == m.ID {
				p.hist[chat][i] = m
			}
		}
	}
	p.mu.Unlock()
	if open {
		p.refresh(chat)
	}
}

// --------------------------------------------------------------- a chat

func (p *Plugin) openChat(chat string) {
	if err := p.loadHistory(chat); err != nil {
		p.setStatus("could not load the chat: " + err.Error())
		return
	}
	p.mu.Lock()
	h := p.hist[chat]
	p.mu.Unlock()
	if len(h) > 0 {
		p.markRead(chat, h[len(h)-1].ID)
	}
}

func (p *Plugin) loadHistory(chat string) error {
	peer, err := p.inputPeer(chat)
	if err != nil {
		return err
	}
	p.mu.Lock()
	api := p.api
	p.mu.Unlock()
	if api == nil {
		return errors.New("not connected")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	res, err := api.MessagesGetHistory(ctx, &tg.MessagesGetHistoryRequest{Peer: peer, Limit: chatLines})
	if err != nil {
		return err
	}
	var msgs []tg.MessageClass
	switch r := res.(type) {
	case *tg.MessagesMessages:
		p.remember(r.Users, r.Chats)
		msgs = r.Messages
	case *tg.MessagesMessagesSlice:
		p.remember(r.Users, r.Chats)
		msgs = r.Messages
	case *tg.MessagesChannelMessages:
		p.remember(r.Users, r.Chats)
		msgs = r.Messages
	}
	var h []*tg.Message
	for i := len(msgs) - 1; i >= 0; i-- {
		if m, ok := msgs[i].(*tg.Message); ok {
			h = append(h, m)
		}
	}
	p.mu.Lock()
	if !p.open[chat] {
		p.mu.Unlock()
		return nil
	}
	p.hist[chat] = h
	p.pending[chat] = nil
	p.mu.Unlock()
	p.refresh(chat)
	return nil
}

// refresh sends the lines of an open chat; pictures load in the background.
func (p *Plugin) refresh(chat string) {
	p.mu.Lock()
	if !p.open[chat] {
		p.mu.Unlock()
		return
	}
	h := append([]*tg.Message(nil), p.hist[chat]...)
	pend := append([]NetLine(nil), p.pending[chat]...)
	p.mu.Unlock()
	lines := []NetLine{}
	for _, m := range h {
		text, kind := describe(m)
		if text == "" && kind == "" {
			continue
		}
		l := NetLine{ID: strconv.Itoa(m.ID), TS: int64(m.Date), FromMe: m.Out, Who: p.sender(m), Text: text, State: ""}
		if m.Out {
			l.State = "sent"
		}
		switch kind {
		case "photo":
			if path := p.mediaPath(m); path != "" {
				if _, err := os.Stat(path); err == nil {
					l.Image = path
				} else if size(m) < autoImage {
					go p.fetchQuiet(chat, m)
				} else {
					l.Actions = []NetAction{{ID: "fetch", Label: "load picture"}}
				}
			}
			if l.Text == "" && l.Image == "" {
				l.Text = "[picture]"
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
	lines = append(lines, pend...)
	p.host.Send(map[string]any{"t": "lines", "chat": chat, "lines": lines})
}

func dur(s int) string { return fmt.Sprintf("%d:%02d", s/60, s%60) }

// describe turns a message into a chat line; what RogueIM can't show becomes
// a line with a link (PRD HB-8).
func describe(m *tg.Message) (string, string) {
	caption := func(c string) string {
		if c == "" {
			return ""
		}
		return "\n" + c
	}
	switch md := m.Media.(type) {
	case nil, *tg.MessageMediaEmpty, *tg.MessageMediaWebPage:
		return m.Message, ""
	case *tg.MessageMediaPhoto:
		return m.Message, "photo"
	case *tg.MessageMediaDocument:
		doc, ok := md.Document.(*tg.Document)
		if !ok {
			return "[file]" + caption(m.Message), ""
		}
		name := ""
		for _, a := range doc.Attributes {
			switch a := a.(type) {
			case *tg.DocumentAttributeSticker:
				return "[sticker " + a.Alt + "]", ""
			case *tg.DocumentAttributeAudio:
				if a.Voice {
					return "[voice message " + dur(a.Duration) + "]", "audio"
				}
				return "[audio " + dur(a.Duration) + "]" + caption(m.Message), "audio"
			case *tg.DocumentAttributeVideo:
				return "[video " + dur(int(a.Duration)) + "]" + caption(m.Message), "video"
			case *tg.DocumentAttributeAnimated:
				return "[GIF]" + caption(m.Message), "video"
			case *tg.DocumentAttributeFilename:
				name = a.FileName
			}
		}
		return "[file: " + name + "]" + caption(m.Message), "doc"
	case *tg.MessageMediaGeo:
		if g, ok := md.Geo.(*tg.GeoPoint); ok {
			return fmt.Sprintf("[location %.5f, %.5f]", g.Lat, g.Long), "location"
		}
	case *tg.MessageMediaVenue:
		if g, ok := md.Geo.(*tg.GeoPoint); ok {
			return fmt.Sprintf("[location %.5f, %.5f]\n%s", g.Lat, g.Long, md.Title), "location"
		}
	case *tg.MessageMediaGeoLive:
		return "[live location]", ""
	case *tg.MessageMediaContact:
		return "[contact: " + strings.TrimSpace(md.FirstName+" "+md.LastName) + " " + md.PhoneNumber + "]", ""
	case *tg.MessageMediaPoll:
		return "[poll: " + md.Poll.Question.Text + "]", ""
	case *tg.MessageMediaDice:
		return fmt.Sprintf("[%s %d]", md.Emoticon, md.Value), ""
	}
	if m.Message != "" {
		return m.Message, ""
	}
	return "[message RogueIM can't show; open Telegram]", ""
}

// markRead marks a chat read up to a message, here and on Telegram.
func (p *Plugin) markRead(chat string, maxID int) {
	p.setUnread(chat, 0)
	peer, err := p.inputPeer(chat)
	p.mu.Lock()
	api := p.api
	p.mu.Unlock()
	if err != nil || api == nil {
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()
	if c, ok := peer.(*tg.InputPeerChannel); ok {
		_, _ = api.ChannelsReadHistory(ctx, &tg.ChannelsReadHistoryRequest{Channel: &tg.InputChannel{ChannelID: c.ChannelID, AccessHash: c.AccessHash}, MaxID: maxID})
		return
	}
	if res, err := api.MessagesReadHistory(ctx, &tg.MessagesReadHistoryRequest{Peer: peer, MaxID: maxID}); err == nil {
		p.mu.Lock()
		gaps := p.gaps
		p.mu.Unlock()
		if gaps != nil {
			_ = gaps.HandleAffected(ctx, 0, res.Pts, res.PtsCount)
		}
	}
}

// --------------------------------------------------------------- sending

func (p *Plugin) addPending(chat, text string) string {
	p.mu.Lock()
	defer p.mu.Unlock()
	id := fmt.Sprintf("p:%d", time.Now().UnixNano())
	p.pending[chat] = append(p.pending[chat], NetLine{ID: id, TS: time.Now().Unix(), FromMe: true, Who: "me", Text: text, State: "sending"})
	return id
}

func (p *Plugin) failPending(chat, id string, err error) {
	p.mu.Lock()
	for i, l := range p.pending[chat] {
		if l.ID == id {
			p.pending[chat][i].State = "failed"
			p.pending[chat][i].Text += "\n[not sent: " + err.Error() + "]"
		}
	}
	p.mu.Unlock()
	p.refresh(chat)
}

// sent passes Telegram's answer on and reloads the chat, which shows the
// message as Telegram stored it.
func (p *Plugin) sent(ctx context.Context, chat string, res tg.UpdatesClass) {
	p.mu.Lock()
	gaps := p.gaps
	p.mu.Unlock()
	if gaps != nil && res != nil {
		_ = gaps.Handle(ctx, res)
	}
	_ = p.loadHistory(chat)
}

func randomID() int64 { return time.Now().UnixNano() }

func (p *Plugin) sendText(chat, body string) {
	pid := p.addPending(chat, body)
	p.refresh(chat)
	peer, err := p.inputPeer(chat)
	p.mu.Lock()
	api := p.api
	p.mu.Unlock()
	if err == nil && api == nil {
		err = errors.New("not connected")
	}
	if err != nil {
		p.failPending(chat, pid, err)
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), 30*time.Second)
	defer cancel()
	res, err := api.MessagesSendMessage(ctx, &tg.MessagesSendMessageRequest{Peer: peer, Message: body, RandomID: randomID()})
	if err != nil {
		p.failPending(chat, pid, err)
		return
	}
	p.sent(ctx, chat, res)
}

func (p *Plugin) sendFile(chat, path string) {
	name := filepath.Base(path)
	ext := strings.ToLower(filepath.Ext(path))
	isImage := ext == ".jpg" || ext == ".jpeg" || ext == ".png"
	label := "[file: " + name + "]"
	if isImage {
		label = "[picture]"
	}
	pid := p.addPending(chat, label)
	p.refresh(chat)
	peer, err := p.inputPeer(chat)
	p.mu.Lock()
	api := p.api
	p.mu.Unlock()
	if err == nil && api == nil {
		err = errors.New("not connected")
	}
	if err != nil {
		p.failPending(chat, pid, err)
		return
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Minute)
	defer cancel()
	file, err := uploader.NewUploader(api).FromPath(ctx, path)
	if err != nil {
		p.failPending(chat, pid, err)
		return
	}
	var media tg.InputMediaClass
	if isImage {
		media = &tg.InputMediaUploadedPhoto{File: file}
	} else {
		mt := mime.TypeByExtension(ext)
		if mt == "" {
			mt = "application/octet-stream"
		}
		media = &tg.InputMediaUploadedDocument{File: file, MimeType: mt, Attributes: []tg.DocumentAttributeClass{&tg.DocumentAttributeFilename{FileName: name}}}
	}
	res, err := api.MessagesSendMedia(ctx, &tg.MessagesSendMediaRequest{Peer: peer, Media: media, RandomID: randomID()})
	if err != nil {
		p.failPending(chat, pid, err)
		return
	}
	p.sent(ctx, chat, res)
}

// --------------------------------------------------------------- media

func size(m *tg.Message) int64 {
	switch md := m.Media.(type) {
	case *tg.MessageMediaDocument:
		if d, ok := md.Document.(*tg.Document); ok {
			return d.Size
		}
	case *tg.MessageMediaPhoto:
		_, s := photoSize(md)
		return int64(s)
	}
	return 0
}

// photoSize picks the biggest version of a photo up to 1280 pixels.
func photoSize(md *tg.MessageMediaPhoto) (string, int) {
	ph, ok := md.Photo.(*tg.Photo)
	if !ok {
		return "", 0
	}
	best, bestW, bestSize := "", 0, 0
	for _, s := range ph.Sizes {
		var t string
		var w, h, n int
		switch s := s.(type) {
		case *tg.PhotoSize:
			t, w, h, n = s.Type, s.W, s.H, s.Size
		case *tg.PhotoSizeProgressive:
			t, w, h = s.Type, s.W, s.H
			if len(s.Sizes) > 0 {
				n = s.Sizes[len(s.Sizes)-1]
			}
		default:
			continue
		}
		if h > w {
			w = h
		}
		if (w <= 1280 && w > bestW) || best == "" {
			best, bestW, bestSize = t, w, n
		}
	}
	return best, bestSize
}

// mediaPath is where a message's media is kept once fetched.
func (p *Plugin) mediaPath(m *tg.Message) string {
	switch md := m.Media.(type) {
	case *tg.MessageMediaPhoto:
		if ph, ok := md.Photo.(*tg.Photo); ok {
			return filepath.Join(p.dir, "media", fmt.Sprintf("p%d.jpg", ph.ID))
		}
	case *tg.MessageMediaDocument:
		if d, ok := md.Document.(*tg.Document); ok {
			ext := ""
			for _, a := range d.Attributes {
				if f, ok := a.(*tg.DocumentAttributeFilename); ok {
					ext = filepath.Ext(f.FileName)
				}
			}
			if ext == "" {
				if exts, _ := mime.ExtensionsByType(d.MimeType); len(exts) > 0 {
					ext = exts[len(exts)-1]
				}
			}
			if strings.ContainsAny(ext, `/\:`) {
				ext = ""
			}
			return filepath.Join(p.dir, "media", fmt.Sprintf("d%d%s", d.ID, ext))
		}
	}
	return ""
}

// fetch downloads a message's media into the plugin folder.
func (p *Plugin) fetch(m *tg.Message) (string, error) {
	path := p.mediaPath(m)
	if path == "" {
		return "", errors.New("nothing to load")
	}
	if _, err := os.Stat(path); err == nil {
		return path, nil
	}
	var loc tg.InputFileLocationClass
	switch md := m.Media.(type) {
	case *tg.MessageMediaPhoto:
		ph := md.Photo.(*tg.Photo)
		t, _ := photoSize(md)
		loc = &tg.InputPhotoFileLocation{ID: ph.ID, AccessHash: ph.AccessHash, FileReference: ph.FileReference, ThumbSize: t}
	case *tg.MessageMediaDocument:
		d := md.Document.(*tg.Document)
		loc = &tg.InputDocumentFileLocation{ID: d.ID, AccessHash: d.AccessHash, FileReference: d.FileReference}
	}
	p.mu.Lock()
	api := p.api
	p.mu.Unlock()
	if api == nil {
		return "", errors.New("not connected")
	}
	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Minute)
	defer cancel()
	// Into memory, then sealed: no plain copy touches the disk.
	var buf bytes.Buffer
	if _, err := downloader.NewDownloader().Download(api, loc).Stream(ctx, &buf); err != nil {
		return "", err
	}
	return path, writeSealed(path, buf.Bytes())
}

// sealedSession keeps the Telegram login sealed like the other plugin files.
type sealedSession struct{ path string }

func (s sealedSession) LoadSession(context.Context) ([]byte, error) {
	b, err := readSealed(s.path)
	if errors.Is(err, os.ErrNotExist) || (err == nil && len(b) == 0) {
		return nil, session.ErrNotFound
	}
	return b, err
}

func (s sealedSession) StoreSession(_ context.Context, data []byte) error {
	return writeSealed(s.path, data)
}

// fetchQuiet loads a picture for the chat once, without bothering anyone.
func (p *Plugin) fetchQuiet(chat string, m *tg.Message) {
	key := p.mediaPath(m)
	p.mu.Lock()
	if p.loading[key] {
		p.mu.Unlock()
		return
	}
	p.loading[key] = true
	p.mu.Unlock()
	_, err := p.fetch(m)
	p.mu.Lock()
	delete(p.loading, key)
	p.mu.Unlock()
	if err == nil {
		p.refresh(chat)
	}
}

func (p *Plugin) action(chat, line, id string) {
	n, _ := strconv.Atoi(line)
	var m *tg.Message
	p.mu.Lock()
	for _, o := range p.hist[chat] {
		if o.ID == n {
			m = o
		}
	}
	p.mu.Unlock()
	if m == nil {
		return
	}
	switch id {
	case "map":
		var lat, lon float64
		if _, err := fmt.Sscanf(strings.SplitN(describeText(m), "\n", 2)[0], "[location %f, %f]", &lat, &lon); err == nil {
			openPath(fmt.Sprintf("https://www.openstreetmap.org/?mlat=%f&mlon=%f#map=16/%f/%f", lat, lon, lat, lon))
		}
	case "fetch", "open", "save":
		path, err := p.fetch(m)
		if err != nil {
			p.setStatus("could not load it: " + err.Error())
			return
		}
		switch id {
		case "fetch":
			p.refresh(chat)
		// The cache is sealed: the viewer gets a plain copy in a temp folder that
		// is emptied when the plugin stops; "save" puts one into Downloads.
		case "open":
			if out, err := plainCopy(path, openDir(), fileName(m)); err == nil {
				openPath(out)
			}
		case "save":
			if out, err := plainCopy(path, downloadsDir(), fileName(m)); err == nil {
				p.setStatus("saved to " + out)
				openPath(filepath.Dir(out))
			}
		}
	}
}

// fileName is a document's own name, or "" to use the cache name.
func fileName(m *tg.Message) string {
	md, ok := m.Media.(*tg.MessageMediaDocument)
	if !ok {
		return ""
	}
	d, ok := md.Document.(*tg.Document)
	if !ok {
		return ""
	}
	for _, a := range d.Attributes {
		if f, ok := a.(*tg.DocumentAttributeFilename); ok && !strings.ContainsAny(f.FileName, `/\:`) {
			return f.FileName
		}
	}
	return ""
}

func describeText(m *tg.Message) string {
	t, _ := describe(m)
	return t
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
