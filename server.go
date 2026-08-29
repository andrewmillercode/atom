// The atom session server. It listens on a Unix socket in the atom data
// directory and manages chat sessions: creating, listing, and deleting
// them, and handling chat turns (model API calls plus tool
// execution) with NDJSON streaming back to the client.
//
// The server is started on demand by the client (atom --serve) and runs
// in the background until killed. Only one server instance exists at a
// time; if a second instance tries to start, it detects the existing one
// and exits cleanly.
package main

import (
	"bufio"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"log"
	"net"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"sync/atomic"
	"syscall"
	"time"
)

// Idle shutdown: the server exits when no client connections have been
// open for idleShutdownAfter. Every running TUI holds a long-lived /events
// subscription, so a visible atom instance keeps the server alive; once
// the last instance quits, the server cleans up after itself.
const idleShutdownAfter = 60 * time.Second

var (
	activeConns atomic.Int64
	idleMu      sync.Mutex
	idleSince   time.Time
)

// noteConnActive marks a connection as open, pausing the idle countdown.
func noteConnOpen() {
	activeConns.Add(1)
}

// noteConnClosed marks a connection as closed. When the last connection
// drops, the idle countdown starts.
func noteConnClosed() {
	if activeConns.Add(-1) == 0 {
		idleMu.Lock()
		idleSince = time.Now()
		idleMu.Unlock()
	}
}

// connTracker wraps the mux so every request (including the long-lived
// SSE /events streams) counts as an active connection while in flight.
func connTracker(next http.Handler) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		noteConnOpen()
		defer noteConnClosed()
		next.ServeHTTP(w, r)
	})
}

// idleMonitor exits the server once it has had zero connections for the
// full idle window. Sessions are persisted to disk on every update, so
// exiting is safe; the next client run starts a fresh server. The
// socket file is only removed when no other server answers on the path:
// a racing server (from a previous bug) may own it, and unlinking it
// would strand that server's listeners.
func idleMonitor() {
	ticker := time.NewTicker(10 * time.Second)
	defer ticker.Stop()
	for range ticker.C {
		if activeConns.Load() != 0 {
			continue
		}
		idleMu.Lock()
		idle := time.Since(idleSince) >= idleShutdownAfter
		idleMu.Unlock()
		if !idle {
			continue
		}
		log.Printf("atom server idle for %s with no connections, shutting down", idleShutdownAfter)
		// Only unlink the socket if nobody else is listening on it;
		// otherwise a live server would be left with an unreachable
		// listener (its clients see "no such file or directory").
		if conn, err := net.Dial("unix", socketPath()); err != nil {
			os.Remove(socketPath())
		} else {
			conn.Close()
		}
		os.Remove(filepath.Join(dataDir(), "server.pid"))
		os.Exit(0)
	}
}

// listenOnSocket creates the Unix socket listener. It returns (nil, nil)
// when another live server is already listening on the path, so the
// caller exits cleanly. A stale socket (from a crashed server) is
// removed and the bind retried once. Binding before removing anything
// makes it impossible for a starter to unlink a live server's socket:
// the bind only succeeds when the path is actually free, and when it
// fails the live server is detected by dialing the path.
func listenOnSocket() (net.Listener, error) {
	for attempt := 0; ; attempt++ {
		l, err := net.Listen("unix", socketPath())
		if err == nil {
			return l, nil
		}
		// The bind failed. If a live server answers on the path,
		// defer to it instead of disturbing its socket.
		if conn, derr := net.Dial("unix", socketPath()); derr == nil {
			conn.Close()
			return nil, nil
		}
		// Stale socket from a crashed server: remove it and retry once.
		if attempt > 0 {
			return nil, err
		}
		os.Remove(socketPath())
	}
}

// sessionSubs maps session IDs to their active subscriber channels.
// When handleSend processes a turn, it broadcasts each event to all
// subscribers so other atom instances viewing the same session see
// updates in real time. subsMu guards the map: subscribers are added
// and removed from /events handler goroutines while handleSend
// broadcasts, so unsynchronized access crashes the server with
// "concurrent map read and map write".
var (
	sessionSubs = map[string][]chan map[string]string{}
	subsMu      sync.Mutex
)

// subscribeSession registers a new subscriber for a session and returns
// the channel that events will be sent to.
func subscribeSession(id string) chan map[string]string {
	sub := make(chan map[string]string, 64)
	subsMu.Lock()
	sessionSubs[id] = append(sessionSubs[id], sub)
	subsMu.Unlock()
	return sub
}

// unsubscribeSession removes a subscriber channel from a session. When
// the last subscriber leaves, any active turn for the session is
// cancelled so generation doesn't keep running with no client listening.
func unsubscribeSession(id string, sub chan map[string]string) {
	subsMu.Lock()
	var remaining []chan map[string]string
	for _, s := range sessionSubs[id] {
		if s != sub {
			remaining = append(remaining, s)
		}
	}
	if len(remaining) == 0 {
		delete(sessionSubs, id)
	} else {
		sessionSubs[id] = remaining
	}
	subsMu.Unlock()
	if len(remaining) == 0 {
		cancelSessionTurns(id)
	}
}

// broadcastSession sends an event to all subscribers of a session.
// It never blocks — if a subscriber's channel is full, the event is dropped.
func broadcastSession(id string, event map[string]string) {
	subsMu.Lock()
	subs := append([]chan map[string]string(nil), sessionSubs[id]...)
	subsMu.Unlock()
	for _, sub := range subs {
		select {
		case sub <- event:
		default: // drop if full
		}
	}
}

// sessionTurns tracks the cancel functions of each session's active
// turns. Pausing a session cancels them so model generation stops
// immediately when no client is listening or a client asks to pause.
type sessionTurn struct {
	turnID string
	cancel context.CancelFunc // cancels the whole turn (Esc / pause)

	// roundMu guards the current provider request so /compact can
	// interrupt generation without ending the turn. The next loop
	// iteration then folds history and resumes.
	roundMu       sync.Mutex
	roundCancel   context.CancelFunc
	compactQueued bool
	compactInstr  string
}

var (
	turnMu        sync.Mutex
	sessionTurns  = map[string][]*sessionTurn{}
	pendingPauses = map[string][]string{}
)

// startTurn registers a cancellable context for a session's turn and
// returns it. If a pause for this turn arrived before the turn
// registered, the context is cancelled immediately.
func startTurn(id, turnID string, parent context.Context) (*sessionTurn, context.Context) {
	ctx, cancel := context.WithCancel(parent)
	t := &sessionTurn{turnID: turnID, cancel: cancel}
	turnMu.Lock()
	for i, p := range pendingPauses[id] {
		if p == turnID {
			pendingPauses[id] = append(pendingPauses[id][:i], pendingPauses[id][i+1:]...)
			cancel()
			break
		}
	}
	if len(pendingPauses[id]) == 0 {
		delete(pendingPauses, id)
	}
	sessionTurns[id] = append(sessionTurns[id], t)
	turnMu.Unlock()
	return t, ctx
}

func (t *sessionTurn) setRoundCancel(c context.CancelFunc) {
	t.roundMu.Lock()
	t.roundCancel = c
	t.roundMu.Unlock()
}

// queueCompact asks the in-flight turn to fold history. If a model
// request is streaming, it is cancelled so the loop can compact and
// resume; the turn itself stays alive.
func (t *sessionTurn) queueCompact(instructions string) {
	t.roundMu.Lock()
	t.compactQueued = true
	t.compactInstr = instructions
	cancel := t.roundCancel
	t.roundMu.Unlock()
	if cancel != nil {
		cancel()
	}
}

func (t *sessionTurn) takeCompact() (string, bool) {
	t.roundMu.Lock()
	defer t.roundMu.Unlock()
	if !t.compactQueued {
		return "", false
	}
	t.compactQueued = false
	s := t.compactInstr
	t.compactInstr = ""
	return s, true
}

// requestSessionCompact queues compaction on the session's latest turn.
// It returns false when no turn is running.
func requestSessionCompact(id, instructions string) bool {
	turnMu.Lock()
	turns := sessionTurns[id]
	turnMu.Unlock()
	if len(turns) == 0 {
		return false
	}
	turns[len(turns)-1].queueCompact(instructions)
	return true
}

// endTurn removes a finished turn from the registry.
func endTurn(id string, t *sessionTurn) {
	turnMu.Lock()
	turns := sessionTurns[id]
	for i, cur := range turns {
		if cur == t {
			sessionTurns[id] = append(turns[:i], turns[i+1:]...)
			break
		}
	}
	if len(sessionTurns[id]) == 0 {
		delete(sessionTurns, id)
	}
	turnMu.Unlock()
}

// cancelSessionTurns cancels every active turn for a session. Used when
// the last subscriber leaves so generation doesn't keep running with no
// client listening.
func cancelSessionTurns(id string) {
	turnMu.Lock()
	turns := sessionTurns[id]
	delete(sessionTurns, id)
	delete(pendingPauses, id)
	turnMu.Unlock()
	for _, t := range turns {
		t.cancel()
	}
}

// pauseSession cancels the active turn with the given turn ID for a
// session. If the turn hasn't registered yet (the pause raced ahead of
// the send), the pause is remembered and applied when the turn starts.
func pauseSession(id, turnID string) {
	turnMu.Lock()
	turns := sessionTurns[id]
	var remaining []*sessionTurn
	cancelled := false
	for _, t := range turns {
		if turnID == "" || t.turnID == turnID {
			t.cancel()
			cancelled = true
		} else {
			remaining = append(remaining, t)
		}
	}
	if len(remaining) == 0 {
		delete(sessionTurns, id)
	} else {
		sessionTurns[id] = remaining
	}
	if !cancelled && turnID != "" {
		pendingPauses[id] = append(pendingPauses[id], turnID)
	}
	turnMu.Unlock()
}

// runServer starts the atom session server. It returns when the server
// shuts down (via SIGTERM, SIGINT, or a listener error).
func runServer() error {
	// If a server is already running, exit cleanly. This handles the race
	// where two clients try to start a server at the same time.
	if conn, err := net.Dial("unix", socketPath()); err == nil {
		conn.Close()
		return nil
	}

	store, err := NewSessionStore()
	if err != nil {
		return fmt.Errorf("session store: %w", err)
	}

	mux := http.NewServeMux()

	// POST /api/sessions   — create a new session
	// GET  /api/sessions   — list all sessions
	mux.HandleFunc("/api/sessions", func(w http.ResponseWriter, r *http.Request) {
		switch r.Method {
		case http.MethodPost:
			var body struct {
				Model string `json:"model"`
				Cwd   string `json:"cwd"`
			}
			json.NewDecoder(r.Body).Decode(&body)
			if body.Cwd == "" {
				body.Cwd, _ = os.Getwd()
			}
			instructions := loadInstructionsFrom(body.Cwd)
			sess := store.Create(body.Model, body.Cwd, instructions)
			writeJSON(w, sess.info())

		case http.MethodGet:
			var infos []SessionInfo
			for _, sess := range store.List() {
				// Skip unstarted sessions (created but never messaged)
				// so the session picker only lists conversations.
				if len(sess.Messages) == 0 {
					continue
				}
				infos = append(infos, sess.info())
			}
			if infos == nil {
				infos = []SessionInfo{}
			}
			writeJSON(w, infos)

		default:
			http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		}
	})

	// GET /api/stats?days=N — aggregated token usage across all sessions
	// (see stats.go). days=0 or absent means all time.
	mux.HandleFunc("/api/stats", func(w http.ResponseWriter, r *http.Request) {
		handleStats(w, r, store)
	})

	// GET /api/capabilities — feature flags so a newer client can detect
	// a stale background server and restart it.
	mux.HandleFunc("/api/capabilities", func(w http.ResponseWriter, r *http.Request) {
		if r.Method != http.MethodGet {
			http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
			return
		}
		writeJSON(w, map[string]bool{"compact": true})
	})

	// /api/sessions/{id}         — GET: fetch, DELETE: delete
	// /api/sessions/{id}/send    — POST: send a message (NDJSON stream)
	// /api/sessions/{id}/compact — POST: fold history into a summary
	mux.HandleFunc("/api/sessions/", func(w http.ResponseWriter, r *http.Request) {
		rest := strings.TrimPrefix(r.URL.Path, "/api/sessions/")
		parts := strings.SplitN(rest, "/", 2)
		id := parts[0]
		if id == "" {
			http.Error(w, "missing session id", http.StatusBadRequest)
			return
		}

		// Sub-path actions.
		if len(parts) == 2 {
			switch parts[1] {
			case "send":
				if r.Method != http.MethodPost {
					http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
					return
				}
				handleSend(store, w, r, id)
				return
			case "events":
				if r.Method != http.MethodGet {
					http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
					return
				}
				handleEvents(w, r, id)
				return
			case "pause":
				if r.Method != http.MethodPost {
					http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
					return
				}
				var body struct {
					TurnID string `json:"turn_id"`
				}
				json.NewDecoder(r.Body).Decode(&body)
				pauseSession(id, body.TurnID)
				broadcastSession(id, map[string]string{"type": "paused"})
				w.WriteHeader(http.StatusNoContent)
				return
			case "compact":
				if r.Method != http.MethodPost {
					http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
					return
				}
				handleCompact(store, w, r, id)
				return
			default:
				http.Error(w, "not found", http.StatusNotFound)
				return
			}
		}

		// /api/sessions/{id}
		sess := store.Get(id)
		if sess == nil {
			http.Error(w, "session not found", http.StatusNotFound)
			return
		}
		switch r.Method {
		case http.MethodGet:
			writeJSON(w, sess)
		case http.MethodPatch:
			// Change the model the session uses. The conversation history
			// stays; only the model that answers future turns changes.
			var body struct {
				Model string `json:"model"`
			}
			json.NewDecoder(r.Body).Decode(&body)
			if body.Model == "" {
				http.Error(w, "model is required", http.StatusBadRequest)
				return
			}
			store.UpdateModel(id, body.Model)
			// Tell other instances viewing this session to reload so they
			// show the new model too.
			broadcastSession(id, map[string]string{"type": "saved"})
			w.WriteHeader(http.StatusNoContent)
		case http.MethodDelete:
			store.Delete(id)
			w.WriteHeader(http.StatusNoContent)
		default:
			http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		}
	})

	// Bind the Unix socket, tolerating a stale socket from a crashed
	// server and exiting cleanly when another live server is running.
	// Binding before removing anything guarantees a racing starter can
	// never unlink a live server's socket file (which orphans the
	// listener and makes every new client dial fail with
	// "no such file or directory").
	listener, err := listenOnSocket()
	if err != nil {
		return fmt.Errorf("listen: %w", err)
	}
	if listener == nil {
		return nil // another server is already running
	}
	os.Chmod(socketPath(), 0600)

	// Write a PID file so the server can be found and managed.
	os.WriteFile(filepath.Join(dataDir(), "server.pid"),
		[]byte(fmt.Sprintf("%d", os.Getpid())), 0644)

	server := &http.Server{Handler: connTracker(mux)}

	// Start the idle countdown now; the first connection pauses it.
	idleMu.Lock()
	idleSince = time.Now()
	idleMu.Unlock()
	go idleMonitor()

	// Graceful shutdown on SIGTERM or SIGINT.
	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, syscall.SIGTERM, syscall.SIGINT)
	go func() {
		<-sigCh
		os.Remove(socketPath())
		os.Remove(filepath.Join(dataDir(), "server.pid"))
		server.Shutdown(context.Background())
	}()

	log.Printf("atom server listening on %s", socketPath())
	return server.Serve(listener)
}

// handleEvents streams NDJSON events to a subscriber in real time. The
// connection stays open until the client disconnects. This lets other
// atom instances viewing the same session see updates as they happen.
func handleEvents(w http.ResponseWriter, r *http.Request, id string) {
	// Subscribe and start streaming. If the session doesn't exist, no
	// events will come; the client will handle reconnection.
	w.Header().Set("Content-Type", "application/x-ndjson")
	w.WriteHeader(http.StatusOK)
	flusher, _ := w.(http.Flusher)

	sub := subscribeSession(id)
	defer unsubscribeSession(id, sub)

	// Send an initial "subscribed" event so the client knows it's connected.
	writeNDJSON(w, flusher, map[string]string{"type": "subscribed"})
	if flusher != nil {
		flusher.Flush()
	}

	// Stream events to the client until it disconnects.
	for {
		select {
		case event := <-sub:
			writeNDJSON(w, flusher, event)
			if flusher != nil {
				flusher.Flush()
			}
		case <-r.Context().Done():
			return
		}
	}
}

// handleSend processes one chat turn: it appends the user's message,
// calls the model API, streams content/reasoning/tool events back to
// the client as NDJSON, executes any tool calls, and loops until the
// model gives a final answer. The session is persisted at the end.
// The turn can be paused at any point: when the client disconnects,
// when the last subscriber leaves, or when a pause request arrives.
func handleSend(store *SessionStore, w http.ResponseWriter, r *http.Request, id string) {
	sess := store.Get(id)
	if sess == nil {
		http.Error(w, "session not found", http.StatusNotFound)
		return
	}

	var body struct {
		Message               string      `json:"message"`
		Thinking              string      `json:"thinking"`
		Key                   string      `json:"key"`
		BaseURL               string      `json:"base_url"`
		ReasoningField        string      `json:"reasoning_field"`
		TurnID                string      `json:"turn_id"`
		Images                []imageData `json:"images"`
		Compact               bool        `json:"compact"`
		CompactInstructions   string      `json:"compact_instructions"`
	}
	if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
		http.Error(w, "invalid body: "+err.Error(), http.StatusBadRequest)
		return
	}

	// Sanity-check attached images before the turn starts: sizes are
	// capped so the request stays reasonable, and a MIME type is required
	// to build the provider's data URL.
	for i, img := range body.Images {
		if img.MIME == "" {
			http.Error(w, fmt.Sprintf("images[%d]: missing mime type", i), http.StatusBadRequest)
			return
		}
		if len(img.Data) > maxImageBase64Bytes {
			http.Error(w, fmt.Sprintf("images[%d]: larger than the %d-byte base64 limit", i, maxImageBase64Bytes), http.StatusBadRequest)
			return
		}
	}

	// Resolve the API key: use what the client sent, then fall back to
	// the same sources the client uses.
	key := body.Key
	if key == "" {
		key = os.Getenv("OLLAMA_API_KEY")
	}
	if key == "" {
		key = loadProviderKey("ollama-cloud")
	}

	// Resolve the base URL: use what the client sent, else derive from
	// whether we have a key.
	baseURL := body.BaseURL
	if baseURL == "" {
		if key == "" {
			baseURL = "http://localhost:11434/v1"
		} else {
			baseURL = "https://ollama.com/v1"
		}
	}
	baseURL = strings.TrimSuffix(baseURL, "/")

	// /compact (and auto-fold) can ride on /send with no new user text.
	compactOnly := body.Compact && body.Message == "" && len(body.Images) == 0
	if !compactOnly {
		sess.Messages = append(sess.Messages, message{Role: "user", Content: body.Message, Images: body.Images})
	}

	// Start the NDJSON stream.
	w.Header().Set("Content-Type", "application/x-ndjson")
	w.WriteHeader(http.StatusOK)
	flusher, _ := w.(http.Flusher)

	// Register the turn so it can be paused. The context is cancelled
	// when the client disconnects, the last subscriber leaves, or a
	// pause request arrives.
	turn, turnCtx := startTurn(id, body.TurnID, r.Context())
	defer endTurn(id, turn)

	tools := toolDefinitions()

	// One failed auto-compact must not be retried on every tool round:
	// the summarizer uses a 10-minute HTTP timeout, so a down Ollama
	// would stall the turn. A manual /compact still forces a fold.
	compactFailed := false

	if compactOnly {
		if err := foldSession(store, sess, w, flusher, id, turnCtx, body.CompactInstructions); err != nil {
			writeNDJSON(w, flusher, map[string]string{"type": "error", "message": "compaction failed: " + err.Error()})
		}
		event := map[string]string{"type": "done"}
		writeNDJSON(w, flusher, event)
		broadcastSession(id, event)
		persistSession(store, sess, id)
		return
	}

	for round := 0; round < 30; round++ {
		// Stop immediately when the turn was paused.
		if turnCtx.Err() != nil {
			finishPausedTurn(store, sess, w, flusher, id)
			return
		}

		extra, forced := turn.takeCompact()
		foldNow := forced || (!compactFailed && shouldCompact(sess))
		if foldNow && !forced {
			_, _, foldNow = compactSpan(sess)
		}
		if foldNow {
			if err := foldSession(store, sess, w, flusher, id, turnCtx, extra); err != nil {
				if !forced {
					compactFailed = true
				}
				writeNDJSON(w, flusher, map[string]string{"type": "error", "message": "compaction failed: " + err.Error()})
			}
		}

		// Build the request: instructions, an optional compaction brief,
		// and the unsummarized tail. Tool calls whose arguments aren't
		// valid JSON are dropped (plus the tool results that answered
		// them); the API rejects requests containing them with 400
		// "invalid tool call arguments".
		msgs := llmMessages(sess)
		reqBody, err := json.Marshal(chatRequest{
			Model:           sess.Model,
			Messages:        msgs,
			Stream:          true,
			Tools:           tools,
			ReasoningEffort: body.Thinking,
			StreamOptions:   &streamOptions{IncludeUsage: true},
		})
		if err != nil {
			writeNDJSON(w, flusher, map[string]string{"type": "error", "message": err.Error()})
			return
		}

		roundCtx, roundCancel := context.WithCancel(turnCtx)
		turn.setRoundCancel(roundCancel)

		req, err := http.NewRequestWithContext(roundCtx, http.MethodPost, baseURL+"/chat/completions", strings.NewReader(string(reqBody)))
		if err != nil {
			roundCancel()
			turn.setRoundCancel(nil)
			writeNDJSON(w, flusher, map[string]string{"type": "error", "message": err.Error()})
			return
		}
		req.Header.Set("Content-Type", "application/json")
		req.Header.Set("Authorization", "Bearer "+key)

		client := &http.Client{Timeout: 10 * time.Minute}
		resp, err := client.Do(req)
		if err != nil {
			roundCancel()
			turn.setRoundCancel(nil)
			if turnCtx.Err() != nil {
				finishPausedTurn(store, sess, w, flusher, id)
				return
			}
			if extra, ok := turn.takeCompact(); ok {
				if ferr := foldSession(store, sess, w, flusher, id, turnCtx, extra); ferr != nil {
					writeNDJSON(w, flusher, map[string]string{"type": "error", "message": "compaction failed: " + ferr.Error()})
				}
				continue
			}
			msg := err.Error()
			if strings.Contains(baseURL, "localhost") {
				msg += " (is Ollama running? or set OLLAMA_API_KEY to talk to ollama.com directly)"
			}
			writeNDJSON(w, flusher, map[string]string{"type": "error", "message": msg})
			return
		}

		if resp.StatusCode != http.StatusOK {
			errMsg, _ := io.ReadAll(io.LimitReader(resp.Body, 4096))
			resp.Body.Close()
			roundCancel()
			turn.setRoundCancel(nil)
			writeNDJSON(w, flusher, map[string]string{"type": "error", "message": resp.Status + ": " + strings.TrimSpace(string(errMsg))})
			return
		}

		// Stream the model's SSE response, relaying each delta to the
		// client as an NDJSON event. The reasoning delta field differs
		// per provider ("reasoning" vs "reasoning_content").
		reasoningField := body.ReasoningField
		if reasoningField == "" {
			reasoningField = reasoningFieldForURL(baseURL)
		}
		result := streamModelToClient(roundCtx, resp.Body, w, flusher, id, reasoningField)
		resp.Body.Close()
		roundCancel()
		turn.setRoundCancel(nil)

		// The provider's token count for this request is the current
		// context snapshot (history, instructions, and tool results).
		// Remember it on the session and tell every viewer, so the
		// status bar indicator updates without waiting for a reload.
		if result.Usage != nil {
			sess.Usage = result.Usage
			usageEvent := map[string]string{
				"type":       "usage",
				"prompt":     strconv.Itoa(result.Usage.PromptTokens),
				"completion": strconv.Itoa(result.Usage.CompletionTokens),
				"total":      strconv.Itoa(result.Usage.TotalTokens),
			}
			writeNDJSON(w, flusher, usageEvent)
			broadcastSession(id, usageEvent)
		}

		// Paused mid-stream: keep the partial reply so nothing is lost.
		if turnCtx.Err() != nil {
			if result.Content != "" || result.Reasoning != "" {
				sess.Messages = append(sess.Messages, assistantMessage(sess.Model, baseURL, result, nil))
			}
			finishPausedTurn(store, sess, w, flusher, id)
			return
		}

		// /compact (or auto-fold) interrupted this round. Keep any
		// partial text, fold, and start a fresh model request.
		if extra, ok := turn.takeCompact(); ok {
			if result.Content != "" || result.Reasoning != "" {
				sess.Messages = append(sess.Messages, assistantMessage(sess.Model, baseURL, result, nil))
			}
			if ferr := foldSession(store, sess, w, flusher, id, turnCtx, extra); ferr != nil {
				writeNDJSON(w, flusher, map[string]string{"type": "error", "message": "compaction failed: " + ferr.Error()})
			}
			continue
		}

		// No tool calls: the model gave its final answer for this turn.
		if len(result.ToolCalls) == 0 {
			sess.Messages = append(sess.Messages, assistantMessage(sess.Model, baseURL, result, nil))
			if extra, ok := turn.takeCompact(); ok {
				if ferr := foldSession(store, sess, w, flusher, id, turnCtx, extra); ferr != nil {
					writeNDJSON(w, flusher, map[string]string{"type": "error", "message": "compaction failed: " + ferr.Error()})
				}
			}
			event := map[string]string{"type": "done"}
			writeNDJSON(w, flusher, event)
			broadcastSession(id, event)
			break
		}

		// Record the assistant's tool-call message so the API accepts
		// the tool results that follow.
		sess.Messages = append(sess.Messages, assistantMessage(sess.Model, baseURL, result, result.ToolCalls))

		// Execute each tool and feed the result back to the model.
		for _, tc := range result.ToolCalls {
			event := map[string]string{"type": "tool", "name": tc.Function.Name, "arguments": tc.Function.Arguments}
			writeNDJSON(w, flusher, event)
			broadcastSession(id, event)
			out, imgs, diff := executeTool(tc.Function.Name, tc.Function.Arguments, key)
			sess.Messages = append(sess.Messages, message{Role: "tool", ToolCallID: tc.ID, Content: out, Images: imgs, Diff: diff})
			// Send any file diff as its own event so the client can attach
			// it to the tool block it already rendered.
			if diff != "" {
				diffEvent := map[string]string{"type": "tool_diff", "diff": diff}
				writeNDJSON(w, flusher, diffEvent)
				broadcastSession(id, diffEvent)
			}
			// Stop between tools when the turn was paused.
			if turnCtx.Err() != nil {
				finishPausedTurn(store, sess, w, flusher, id)
				return
			}
		}
		// Loop again: the model now sees the tool results.
	}

	persistSession(store, sess, id)
}

// assistantMessage builds the persisted record for one model reply: the
// reply text, any tool calls, the provider and model that answered, and
// the request's token usage so the stats report can attribute it. The
// per-message usage record is what makes per-model stats exact even when
// a session switches models mid-conversation.
func assistantMessage(model, baseURL string, result streamResult, toolCalls []toolCall) message {
	return message{
		Role:      "assistant",
		Content:   result.Content,
		Reasoning: result.Reasoning,
		ToolCalls: toolCalls,
		Provider:  providerNameForURL(baseURL),
		Model:     model,
		Usage:     result.Usage,
	}
}

// finishPausedTurn ends a paused turn: it tells the client the stream
// stopped, persists the partial conversation, and notifies other viewers.
func finishPausedTurn(store *SessionStore, sess *Session, w http.ResponseWriter, flusher http.Flusher, id string) {
	event := map[string]string{"type": "paused"}
	writeNDJSON(w, flusher, event)
	broadcastSession(id, event)
	persistSession(store, sess, id)
}

// persistSession saves a session's messages, deriving the title from the
// first user message when none is set, and notifies other viewers.
func persistSession(store *SessionStore, sess *Session, id string) {
	title := ""
	for _, m := range sess.Messages {
		if m.Role == "user" {
			title = m.Content
			if len(title) > 60 {
				title = title[:60] + "..."
			}
			break
		}
	}
	store.Update(id, sess.Messages, title)
	broadcastSession(id, map[string]string{"type": "saved"})
}

// foldSession summarizes older turns and notifies the client with the
// same start/end events the TUI uses for thinking: compaction then
// compaction_end. The transcript on sess.Messages is left intact.
func foldSession(store *SessionStore, sess *Session, w http.ResponseWriter, flusher http.Flusher, id string, ctx context.Context, extra string) error {
	start := map[string]string{"type": "compaction"}
	writeNDJSON(w, flusher, start)
	broadcastSession(id, start)
	baseURL, key := compactionProvider()
	err := compactSession(ctx, sess, nil, baseURL, key, extra)
	end := map[string]string{"type": "compaction_end"}
	if err == nil {
		end["text"] = compactionPromptText(sess.CompactionSummary)
	}
	writeNDJSON(w, flusher, end)
	broadcastSession(id, end)
	if err != nil {
		return err
	}
	if sess.Usage != nil {
		usageEvent := map[string]string{
			"type":       "usage",
			"prompt":     strconv.Itoa(sess.Usage.PromptTokens),
			"completion": strconv.Itoa(sess.Usage.CompletionTokens),
			"total":      strconv.Itoa(sess.Usage.TotalTokens),
		}
		writeNDJSON(w, flusher, usageEvent)
		broadcastSession(id, usageEvent)
	}
	persistSession(store, sess, id)
	return nil
}

// toolCallAccumulator rebuilds complete tool calls from a streamed
// response's deltas. Two provider shapes are handled:
//
//   - OpenAI-style streams fragment one call's fields across many deltas
//     and use a unique Index per call; fragments are concatenated.
//   - Some routers (Ollama) stream each parallel call as a complete
//     arguments object that reuses index 0; a delta whose arguments
//     would corrupt the accumulated string starts a new call instead.
//
// Calls are kept in arrival order so the final list matches the order
// the model emitted them.
type toolCallAccumulator struct {
	calls   []*toolCall
	byIndex map[int]*toolCall
}

func newToolCallAccumulator() *toolCallAccumulator {
	return &toolCallAccumulator{byIndex: map[int]*toolCall{}}
}

// add merges one delta into the accumulator, opening a new call when the
// delta clearly belongs to a different call than the one at its index: a
// different call ID, or a complete JSON arguments object that can't be
// appended to the accumulated string.
func (a *toolCallAccumulator) add(d streamToolCallDelta) {
	existing := a.byIndex[d.Index]
	if existing != nil {
		// A delta naming a different call ID than the one at this index
		// starts a new call (routers reuse index 0 for every call).
		if d.ID != "" && existing.ID != "" && d.ID != existing.ID {
			existing = nil
		} else if d.Function.Arguments != "" && existing.Function.Arguments != "" &&
			json.Valid([]byte(existing.Function.Arguments)) &&
			json.Valid([]byte(d.Function.Arguments)) &&
			!json.Valid([]byte(existing.Function.Arguments+d.Function.Arguments)) {
			// Two complete JSON objects can't form one argument string:
			// the delta is a second call reusing this index.
			existing = nil
		}
	}
	if existing == nil {
		existing = &toolCall{ID: d.ID, Type: d.Type}
		existing.Function.Name = d.Function.Name
		existing.Function.Arguments = d.Function.Arguments
		a.byIndex[d.Index] = existing
		a.calls = append(a.calls, existing)
		return
	}
	// Fill in fields that arrive in later deltas.
	if existing.ID == "" && d.ID != "" {
		existing.ID = d.ID
	}
	if existing.Type == "" && d.Type != "" {
		existing.Type = d.Type
	}
	if existing.Function.Name == "" && d.Function.Name != "" {
		existing.Function.Name = d.Function.Name
	}
	existing.Function.Arguments += d.Function.Arguments
}

// list returns the accumulated tool calls in the order their first
// deltas arrived.
func (a *toolCallAccumulator) list() []toolCall {
	out := make([]toolCall, 0, len(a.calls))
	for _, tc := range a.calls {
		out = append(out, *tc)
	}
	return out
}

// sanitizeMessages returns the messages to send to the model API with
// tool calls that would make the request invalid removed: assistant tool
// calls whose arguments aren't valid JSON (or have no function name),
// and the tool responses that answered them. Providers reject requests
// that contain a malformed tool call (400 "invalid tool call
// arguments"), so a single bad call poisons every later request that
// re-sends the history. Dropping the bad call and its orphaned result
// keeps the conversation usable; the model simply re-issues the call on
// its next turn. The input slice is not modified.
func sanitizeMessages(msgs []message) []message {
	out := make([]message, 0, len(msgs))
	validIDs := map[string]bool{}
	for _, m := range msgs {
		if m.Role == "tool" {
			if !validIDs[m.ToolCallID] {
				continue // orphaned tool result
			}
			out = append(out, m)
			continue
		}
		if m.Role == "assistant" && len(m.ToolCalls) > 0 {
			calls := m.ToolCalls[:0:0]
			for _, tc := range m.ToolCalls {
				if tc.Function.Name == "" || !json.Valid([]byte(tc.Function.Arguments)) {
					continue
				}
				calls = append(calls, tc)
				validIDs[tc.ID] = true
			}
			if len(calls) == 0 {
				continue // drop the empty tool-call turn entirely
			}
			m.ToolCalls = calls
			out = append(out, m)
			continue
		}
		out = append(out, m)
	}
	return out
}

// streamModelToClient reads the SSE response from the model API and
// relays each delta to the client as NDJSON events. It returns the
// full assistant reply and any tool calls, exactly like the client-side
// stream() did before the server split. When ctx is cancelled (the turn
// was paused), it stops reading and returns the partial reply so far.
func streamModelToClient(ctx context.Context, r io.Reader, w http.ResponseWriter, flusher http.Flusher, sessionID string, reasoningField string) streamResult {
	var reply strings.Builder
	var reasoning strings.Builder
	accumulator := newToolCallAccumulator()
	var usage *streamUsage
	reader := bufio.NewReader(r)
	sawReasoning := false
	for {
		// Stop reading when the turn was paused. The cancelled request
		// also closes the body, so this is just a fast path.
		if ctx.Err() != nil {
			break
		}
		line, err := reader.ReadString('\n')
		if line != "" {
			if data, ok := sseData(line); ok {
				if data == "[DONE]" {
					break
				}
				var chunk streamChunk
				if json.Unmarshal([]byte(data), &chunk) != nil {
					continue
				}
				// The final chunk (with stream_options.include_usage)
				// carries the request's token counts.
				if chunk.Usage != nil && chunk.Usage.TotalTokens > 0 {
					usage = chunk.Usage
				}
				for _, choice := range chunk.Choices {
					// Pick the reasoning delta for this provider; fall
					// back to the other field when the configured one
					// is empty (defensive against router changes).
					rt := choice.Delta.Reasoning
					if reasoningField == "reasoning_content" {
						if choice.Delta.ReasoningContent != "" {
							rt = choice.Delta.ReasoningContent
						}
					} else if rt == "" && choice.Delta.ReasoningContent != "" {
						rt = choice.Delta.ReasoningContent
					}
					if rt != "" {
						event := map[string]string{"type": "reasoning", "text": rt}
						writeNDJSON(w, flusher, event)
						broadcastSession(sessionID, event)
						reasoning.WriteString(rt)
						sawReasoning = true
					}
					if choice.Delta.Content != "" {
						if sawReasoning {
							event := map[string]string{"type": "reasoning_end"}
							writeNDJSON(w, flusher, event)
							broadcastSession(sessionID, event)
							sawReasoning = false
						}
						event := map[string]string{"type": "content", "text": choice.Delta.Content}
						writeNDJSON(w, flusher, event)
						broadcastSession(sessionID, event)
						reply.WriteString(choice.Delta.Content)
					}
					// Accumulate tool calls, splitting deltas that
					// reuse an index for a different call.
					for _, tc := range choice.Delta.ToolCalls {
						accumulator.add(tc)
					}
				}
			}
		}
		if err != nil {
			break
		}
	}
	return streamResult{Content: reply.String(), Reasoning: reasoning.String(), ToolCalls: accumulator.list(), Usage: usage}
}

// writeJSON writes a JSON response with the correct content type.
func writeJSON(w http.ResponseWriter, v interface{}) {
	w.Header().Set("Content-Type", "application/json")
	json.NewEncoder(w).Encode(v)
}

// writeNDJSON writes one JSON object followed by a newline, then flushes
// so the client receives it immediately.
func writeNDJSON(w http.ResponseWriter, flusher http.Flusher, v interface{}) {
	json.NewEncoder(w).Encode(v)
	if flusher != nil {
		flusher.Flush()
	}
}