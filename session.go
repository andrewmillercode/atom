// Session management for the atom server. Sessions are persisted as JSON
// files in the atom data directory so they survive server restarts.
package main

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"sync"
	"time"
)

// Session is a persisted conversation. Messages hold the conversation
// history without system instructions, which are loaded fresh from disk
// when the session is created. Model is set at creation time and can be
// changed later (the TUI's /model command updates it); Cwd is fixed for
// the life of the session. Usage is the token count of the session's
// latest model request, reported by the provider and used to render the
// context indicator in the status bar and to decide when to compact.
// CompactionSummary and CompactedThrough fold older turns into a short
// brief for later model requests; the full transcript stays in Messages.
type Session struct {
	ID                string       `json:"id"`
	Title             string       `json:"title"`
	Messages          []message    `json:"messages"`
	Model             string       `json:"model"`
	Cwd               string       `json:"cwd"`
	Instructions      []message    `json:"instructions"`
	Usage             *streamUsage `json:"usage,omitempty"`
	CompactionSummary string       `json:"compaction_summary,omitempty"`
	CompactedThrough  int          `json:"compacted_through,omitempty"` // count of Messages already folded into the summary
	CreatedAt         time.Time    `json:"created_at"`
	UpdatedAt         time.Time    `json:"updated_at"`
}

// SessionInfo is a summary of a session for listing, omitting the full
// message history and instructions.
type SessionInfo struct {
	ID           string       `json:"id"`
	Title        string       `json:"title"`
	Model        string       `json:"model"`
	MessageCount int          `json:"message_count"`
	Usage        *streamUsage `json:"usage,omitempty"`
	CreatedAt    time.Time    `json:"created_at"`
	UpdatedAt    time.Time    `json:"updated_at"`
}

// SessionStore manages sessions in memory and persists each to a JSON
// file in the atom data directory. mu guards the map: handlers run on
// separate goroutines (one per request), so unsynchronized access can
// crash the server with "concurrent map read and map write".
type SessionStore struct {
	dir      string
	mu       sync.Mutex
	sessions map[string]*Session
}

// newSessionID generates a short random hex ID (16 characters).
func newSessionID() string {
	b := make([]byte, 8)
	rand.Read(b)
	return hex.EncodeToString(b)
}

// dataDir returns the atom data directory, honouring XDG_DATA_HOME and
// defaulting to ~/.local/share/atom.
func dataDir() string {
	d := os.Getenv("XDG_DATA_HOME")
	if d == "" {
		home, err := os.UserHomeDir()
		if err != nil {
			return filepath.Join(os.TempDir(), "atom")
		}
		d = filepath.Join(home, ".local", "share")
	}
	return filepath.Join(d, "atom")
}

// socketPath returns the path to the atom server's Unix socket.
func socketPath() string {
	return filepath.Join(dataDir(), "atom.sock")
}

// sessionsDir returns the directory where session JSON files are stored.
func sessionsDir() string {
	return filepath.Join(dataDir(), "sessions")
}

// NewSessionStore creates a session store, loading any existing sessions
// from disk into memory.
func NewSessionStore() (*SessionStore, error) {
	dir := sessionsDir()
	if err := os.MkdirAll(dir, 0755); err != nil {
		return nil, err
	}
	s := &SessionStore{dir: dir, sessions: make(map[string]*Session)}
	s.loadAll()
	return s, nil
}

// loadAll reads all session JSON files from disk into memory.
func (s *SessionStore) loadAll() {
	entries, err := os.ReadDir(s.dir)
	if err != nil {
		return
	}
	for _, e := range entries {
		if e.IsDir() || !strings.HasSuffix(e.Name(), ".json") {
			continue
		}
		b, err := os.ReadFile(filepath.Join(s.dir, e.Name()))
		if err != nil {
			continue
		}
		var sess Session
		if json.Unmarshal(b, &sess) != nil {
			continue
		}
		s.sessions[sess.ID] = &sess
	}
}

// Create makes a new session with the given model, cwd, and instructions,
// persists it, and returns it.
func (s *SessionStore) Create(model, cwd string, instructions []message) *Session {
	sess := &Session{
		ID:           newSessionID(),
		Model:        model,
		Cwd:          cwd,
		Instructions: instructions,
		CreatedAt:    time.Now(),
		UpdatedAt:    time.Now(),
	}
	s.mu.Lock()
	s.sessions[sess.ID] = sess
	s.mu.Unlock()
	s.save(sess)
	return sess
}

// Get returns a session by ID, or nil if not found.
func (s *SessionStore) Get(id string) *Session {
	s.mu.Lock()
	defer s.mu.Unlock()
	return s.sessions[id]
}

// List returns all sessions sorted by UpdatedAt descending (most recent
// first).
func (s *SessionStore) List() []*Session {
	s.mu.Lock()
	list := make([]*Session, 0, len(s.sessions))
	for _, sess := range s.sessions {
		list = append(list, sess)
	}
	s.mu.Unlock()
	sort.Slice(list, func(i, j int) bool {
		return list[i].UpdatedAt.After(list[j].UpdatedAt)
	})
	return list
}

// Update saves the messages for a session and touches its UpdatedAt.
// If title is non-empty, it replaces the session's title.
func (s *SessionStore) Update(id string, messages []message, title string) {
	s.mu.Lock()
	sess, ok := s.sessions[id]
	if ok {
		sess.Messages = messages
		if title != "" {
			sess.Title = title
		}
		sess.UpdatedAt = time.Now()
	}
	s.mu.Unlock()
	if !ok {
		return
	}
	s.save(sess)
}

// UpdateModel changes the model that answers future messages in a session.
// The conversation history is kept; only the model used for new turns
// changes. It's a no-op if the session doesn't exist.
func (s *SessionStore) UpdateModel(id, model string) {
	s.mu.Lock()
	sess, ok := s.sessions[id]
	if ok {
		sess.Model = model
		sess.UpdatedAt = time.Now()
	}
	s.mu.Unlock()
	if ok {
		s.save(sess)
	}
}

// Delete removes a session from memory and disk.
func (s *SessionStore) Delete(id string) {
	s.mu.Lock()
	delete(s.sessions, id)
	s.mu.Unlock()
	os.Remove(filepath.Join(s.dir, id+".json"))
}

// save writes a session to its JSON file.
func (s *SessionStore) save(sess *Session) {
	b, err := json.MarshalIndent(sess, "", "  ")
	if err != nil {
		return
	}
	os.WriteFile(filepath.Join(s.dir, sess.ID+".json"), b, 0644)
}

// info returns a SessionInfo summary for a session. When no title has been
// assigned, the first user message becomes the display name so sessions
// are recognizable in the picker.
func (s *Session) info() SessionInfo {
	title := s.Title
	if title == "" {
		title = sessionName(s.Messages)
	}
	return SessionInfo{
		ID:           s.ID,
		Title:        title,
		Model:        s.Model,
		MessageCount: len(s.Messages),
		Usage:        s.Usage,
		CreatedAt:    s.CreatedAt,
		UpdatedAt:    s.UpdatedAt,
	}
}

// sessionName derives a display name from a session's first user message:
// the first line, truncated to 60 characters. Returns "" for empty sessions.
func sessionName(messages []message) string {
	for _, msg := range messages {
		if msg.Role != "user" {
			continue
		}
		line := msg.Content
		if i := strings.IndexByte(line, '\n'); i >= 0 {
			line = line[:i]
		}
		r := []rune(line)
		if len(r) > 60 {
			r = r[:60]
		}
		return string(r)
	}
	return ""
}
