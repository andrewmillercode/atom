// atom is a chat client backed by a central session server. On startup it
// connects to the server (starting one if none is running), creates or
// resumes a session, and streams replies from the server as NDJSON events.
//
// The server (server.go) handles model API calls, tool execution, and
// session persistence. The client handles terminal I/O, the slash-command
// menu, and rendering streamed events.
package main

import (
	"bytes"
	"context"
	"crypto/sha256"
	"encoding/base64"
	"encoding/hex"
	"encoding/json"
	"flag"
	"fmt"
	"github.com/aymanbagabas/go-udiff"
	"io"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strconv"
	"strings"
	"syscall"
	"time"
)

// imageData is one attached image: base64-encoded file bytes plus their
// MIME type. Images ride along on user messages (pasted into the prompt)
// and tool results (read_file on an image file).
type imageData struct {
	MIME string `json:"mime"`
	Data string `json:"data"` // base64-encoded file bytes
}

// maxImageSourceBytes is how much raw image data we'll ingest (paste,
// drop, or read_file). Oversized sources are refused before decode.
const maxImageSourceBytes = 20 << 20 // 20MB

// maxImageDim and maxImageBase64Bytes match OpenCode's attachment
// limits: scale down to 2000×2000 and keep the base64 payload under 5MiB.
const (
	maxImageDim         = 2000
	maxImageBase64Bytes = 5 << 20 // 5242880
)

// message is one conversation entry. Content is plain text; Images are
// attached pictures that providers receive as image content parts (the
// OpenAI image_url format). The JSON form of Content switches between a
// plain string and a content array depending on whether Images is set
// (see MarshalJSON/UnmarshalJSON).
type message struct {
	Role       string      `json:"role"`
	Content    string      `json:"content"`
	Images     []imageData `json:"images,omitempty"`
	Reasoning  string      `json:"reasoning,omitempty"`
	ToolCalls  []toolCall  `json:"tool_calls,omitempty"`
	ToolCallID string      `json:"tool_call_id,omitempty"`
	Diff       string      `json:"diff,omitempty"`
	// Provider and Model record who answered this message, so stats can
	// attribute usage even after a session switches models.
	Provider string `json:"provider,omitempty"`
	Model    string `json:"model,omitempty"`
	// Usage is the provider's token count for the request that produced
	// this message. Tool-call turns carry their own usage record, so a
	// single user turn with tool calls stores one record per round.
	Usage *streamUsage `json:"usage,omitempty"`
}

// MarshalJSON renders content as a plain string when the message carries
// no images, and as an OpenAI-style content array (a text part followed
// by one image_url part per image) when it does, so vision models
// receive the pictures.
func (m message) MarshalJSON() ([]byte, error) {
	if len(m.Images) == 0 {
		type plain message
		return json.Marshal(plain(m))
	}
	parts := make([]map[string]any, 0, 1+len(m.Images))
	if m.Content != "" {
		parts = append(parts, map[string]any{"type": "text", "text": m.Content})
	}
	for _, img := range m.Images {
		parts = append(parts, map[string]any{
			"type":      "image_url",
			"image_url": map[string]string{"url": "data:" + img.MIME + ";base64," + img.Data},
		})
	}
	out := struct {
		Role       string       `json:"role"`
		Content    interface{}  `json:"content"`
		Reasoning  string       `json:"reasoning,omitempty"`
		ToolCalls  []toolCall   `json:"tool_calls,omitempty"`
		ToolCallID string       `json:"tool_call_id,omitempty"`
		Diff       string       `json:"diff,omitempty"`
		Provider   string       `json:"provider,omitempty"`
		Model      string       `json:"model,omitempty"`
		Usage      *streamUsage `json:"usage,omitempty"`
	}{m.Role, parts, m.Reasoning, m.ToolCalls, m.ToolCallID, m.Diff, m.Provider, m.Model, m.Usage}
	return json.Marshal(out)
}

// UnmarshalJSON accepts both content forms: a plain string, or an array
// of text/image_url parts. Image parts are folded back into Images, so
// persisted sessions round-trip without losing attachments.
func (m *message) UnmarshalJSON(b []byte) error {
	var raw struct {
		Role       string          `json:"role"`
		Content    json.RawMessage `json:"content"`
		Reasoning  string          `json:"reasoning,omitempty"`
		ToolCalls  []toolCall      `json:"tool_calls,omitempty"`
		ToolCallID string          `json:"tool_call_id,omitempty"`
		Diff       string          `json:"diff,omitempty"`
		Provider   string          `json:"provider,omitempty"`
		Model      string          `json:"model,omitempty"`
		Usage      *streamUsage    `json:"usage,omitempty"`
	}
	if err := json.Unmarshal(b, &raw); err != nil {
		return err
	}
	m.Role = raw.Role
	m.Reasoning = raw.Reasoning
	m.ToolCalls = raw.ToolCalls
	m.ToolCallID = raw.ToolCallID
	m.Diff = raw.Diff
	m.Provider = raw.Provider
	m.Model = raw.Model
	m.Usage = raw.Usage

	if len(raw.Content) > 0 && raw.Content[0] == '[' {
		var parts []struct {
			Type     string `json:"type"`
			Text     string `json:"text"`
			ImageURL struct {
				URL string `json:"url"`
			} `json:"image_url"`
		}
		if err := json.Unmarshal(raw.Content, &parts); err != nil {
			return err
		}
		for _, p := range parts {
			switch p.Type {
			case "text":
				m.Content += p.Text
			case "image_url":
				if mime, data, ok := parseDataURL(p.ImageURL.URL); ok {
					m.Images = append(m.Images, imageData{MIME: mime, Data: data})
				}
			}
		}
		return nil
	}
	return json.Unmarshal(raw.Content, &m.Content)
}

// parseDataURL splits "data:<mime>;base64,<data>" into its parts.
func parseDataURL(url string) (mime, data string, ok bool) {
	rest, found := strings.CutPrefix(url, "data:")
	if !found {
		return "", "", false
	}
	head, tail, found := strings.Cut(rest, ";base64,")
	if !found {
		return "", "", false
	}
	return head, tail, true
}

// toolCall is one tool invocation the model requested. Arguments is a
// JSON-encoded string (double-encoded by the model).
type toolCall struct {
	ID       string `json:"id"`
	Type     string `json:"type"`
	Function struct {
		Name      string `json:"name"`
		Arguments string `json:"arguments"`
	} `json:"function"`
}

type chatRequest struct {
	Model           string         `json:"model"`
	Messages        []message      `json:"messages"`
	Stream          bool           `json:"stream"`
	Tools           []toolDef      `json:"tools,omitempty"`
	ReasoningEffort string         `json:"reasoning_effort,omitempty"`
	StreamOptions   *streamOptions `json:"stream_options,omitempty"`
}

// streamOptions asks the provider to include a usage object in the final
// streamed chunk, mirroring what opencode sends. Without it, most
// OpenAI-compatible routers (DeepSeek, OpenCode Go) omit usage from
// streaming responses; Ollama ignores the field and reports usage anyway.
type streamOptions struct {
	IncludeUsage bool `json:"include_usage"`
}

// streamUsage is a provider-reported token count, in OpenAI's usage
// shape. PromptTokens is the full context sent (history, instructions,
// and tool results); TotalTokens is prompt + completion for the request.
// The extra fields are best-effort: ReasoningTokens, CacheReadTokens, and
// CacheWriteTokens are zero when the provider doesn't report them, and
// Cost is only counted when the provider includes a price.
type streamUsage struct {
	PromptTokens     int     `json:"prompt_tokens"`
	CompletionTokens int     `json:"completion_tokens"`
	TotalTokens      int     `json:"total_tokens"`
	ReasoningTokens  int     `json:"reasoning_tokens,omitempty"`
	CacheReadTokens  int     `json:"cache_read_tokens,omitempty"`
	CacheWriteTokens int     `json:"cache_write_tokens,omitempty"`
	Cost             float64 `json:"cost,omitempty"`
}

// UnmarshalJSON parses the provider's raw usage payload, pulling the
// extras out of the fields routers actually send: DeepSeek and OpenCode
// Go report cache hits as prompt_cache_hit_tokens and reasoning inside
// completion_tokens_details, and OpenRouter-style routers report
// total_cost. Unknown fields are ignored.
func (u *streamUsage) UnmarshalJSON(b []byte) error {
	var raw struct {
		PromptTokens     int     `json:"prompt_tokens"`
		CompletionTokens int     `json:"completion_tokens"`
		TotalTokens      int     `json:"total_tokens"`
		CacheReadTokens  int     `json:"prompt_cache_hit_tokens"`
		CacheWriteTokens int     `json:"prompt_cache_miss_tokens"`
		Cost             float64 `json:"total_cost"`
		Details          struct {
			ReasoningTokens int `json:"reasoning_tokens"`
		} `json:"completion_tokens_details"`
	}
	if err := json.Unmarshal(b, &raw); err != nil {
		return err
	}
	u.PromptTokens = raw.PromptTokens
	u.CompletionTokens = raw.CompletionTokens
	u.TotalTokens = raw.TotalTokens
	u.ReasoningTokens = raw.Details.ReasoningTokens
	u.CacheReadTokens = raw.CacheReadTokens
	u.CacheWriteTokens = raw.CacheWriteTokens
	u.Cost = raw.Cost
	return nil
}

// toolDef declares a function the model may call, in OpenAI function-calling
// format. Parameters is kept raw so the JSON schema is embedded verbatim.
type toolDef struct {
	Type     string `json:"type"`
	Function struct {
		Name        string          `json:"name"`
		Description string          `json:"description"`
		Parameters  json.RawMessage `json:"parameters"`
	} `json:"function"`
}

// streamToolCallDelta is one tool-call fragment inside a streamed chunk.
// OpenAI-compatible providers split a single call's fields across many
// deltas, with Index naming which call a fragment belongs to. Some
// routers (Ollama) instead stream each parallel call as a complete
// arguments object that reuses index 0; toolCallAccumulator in server.go
// distinguishes the two shapes.
type streamToolCallDelta struct {
	Index    int    `json:"index"`
	ID       string `json:"id"`
	Type     string `json:"type"`
	Function struct {
		Name      string `json:"name"`
		Arguments string `json:"arguments"`
	} `json:"function"`
}

// streamChunk is one SSE data payload. Only the fields we care about;
// this is the same schema opencode parses for its ollama provider.
type streamChunk struct {
	Choices []struct {
		Delta struct {
			Content          string                `json:"content"`
			Reasoning        string                `json:"reasoning"`         // Ollama-style thinking field
			ReasoningContent string                `json:"reasoning_content"` // OpenCode Go / DeepSeek-style field
			ToolCalls        []streamToolCallDelta `json:"tool_calls"`
		} `json:"delta"`
		FinishReason string `json:"finish_reason"`
	} `json:"choices"`
	// The final chunk carries the request's token counts when the
	// provider honours stream_options.include_usage. Nil until then.
	Usage *streamUsage `json:"usage"`
}

// streamResult is the outcome of one streaming turn: the reply text (empty
// when the model only issued tool calls) plus any tool calls requested.
type streamResult struct {
	Content   string
	Reasoning string
	ToolCalls []toolCall
	Usage     *streamUsage
}

// ANSI color codes for terminal output: grey for the model's reasoning,
// blue for the user's prompt. Codes are skipped when output isn't a terminal.
const (
	colorReset = "\033[0m"
	colorGrey  = "\033[90m" // bright black = light grey
	colorBlue  = "\033[94m" // bright blue = light blue
	colorWhite = "\033[97m" // bright white
)

// isTerminal reports whether f is a terminal, so ANSI codes can be skipped
// when output is piped or redirected.
func isTerminal(f *os.File) bool {
	info, err := f.Stat()
	if err != nil {
		return false
	}
	return info.Mode()&os.ModeCharDevice != 0
}

// paint wraps text in an ANSI color, but only when f is a terminal.
func paint(f *os.File, color, text string) string {
	if !isTerminal(f) {
		return text
	}
	return color + text + colorReset
}

// loadProviderKey reads the API key for the named provider from the providers
// directory. It honours XDG_DATA_HOME and defaults to ~/.local/share/atom.
// Returns "" if the file is missing or unreadable, so callers can fall back
// to other sources.
func loadProviderKey(provider string) string {
	dataDir := os.Getenv("XDG_DATA_HOME")
	if dataDir == "" {
		home, err := os.UserHomeDir()
		if err != nil {
			return ""
		}
		dataDir = home + "/.local/share"
	}
	path := dataDir + "/atom/providers/" + provider
	b, err := os.ReadFile(path)
	if err != nil {
		return ""
	}
	return strings.TrimSpace(string(b))
}

// readInstructionFile reads an AGENTS.md file and renders it as a single
// system-instruction block in OpenCode's format. Read errors are skipped
// silently: instruction files are optional.
func readInstructionFile(path string) (string, bool) {
	b, err := os.ReadFile(path)
	if err != nil {
		return "", false
	}
	content := fmt.Sprintf("Instructions from: %s\n%s", path, strings.TrimSpace(string(b)))
	return content, true
}

// loadInstructionsFrom collects instruction files into system messages for
// a given working directory. It loads AGENTS.md files (global then project,
// matching OpenCode's merge order) and TOOLS.md from the atom config
// directory. The server calls this when creating a session.
func loadInstructionsFrom(cwd string) []message {
	var instructions []message

	// Global source: AGENTS.md and TOOLS.md in the atom config directory
	// ($XDG_CONFIG_HOME/atom, defaulting to ~/.config/atom).
	configDir := os.Getenv("XDG_CONFIG_HOME")
	if configDir == "" {
		if home, err := os.UserHomeDir(); err == nil {
			configDir = home + "/.config"
		}
	}
	if configDir != "" {
		if content, ok := readInstructionFile(configDir + "/atom/AGENTS.md"); ok {
			instructions = append(instructions, message{Role: "system", Content: content})
		}
		if content, ok := readInstructionFile(configDir + "/atom/TOOLS.md"); ok {
			instructions = append(instructions, message{Role: "system", Content: content})
		}
	}

	// Project source: walk from cwd up to the home directory, collecting every
	// AGENTS.md found. Closest-to-cwd files are added first and the root-most
	// last, matching OpenCode's findUp ordering. If cwd is outside home,
	// only cwd itself is checked.
	home, err := os.UserHomeDir()
	if err != nil {
		return instructions
	}
	cwd = filepath.Clean(cwd)
	insideHome := cwd == home || strings.HasPrefix(cwd, home+string(filepath.Separator))
	for {
		if content, ok := readInstructionFile(filepath.Join(cwd, "AGENTS.md")); ok {
			instructions = append(instructions, message{Role: "system", Content: content})
		}
		if cwd == home || !insideHome || cwd == string(filepath.Separator) {
			break
		}
		cwd = filepath.Dir(cwd)
	}
	return instructions
}

func main() {
	serve := flag.Bool("serve", false, "run the session server in the background")
	model := flag.String("model", "", "model to chat with (omit to pick from the model selector)")
	key := flag.String("key", os.Getenv("OLLAMA_API_KEY"), "API key (or set OLLAMA_API_KEY / OPENCODE_GO_API_KEY / OPENCODE_ZEN_API_KEY, or put it in ~/.local/share/atom/providers/)")
	base := flag.String("url", "", "OpenAI-compatible base URL (default: auto-detected from provider)")
	sessionID := flag.String("session", "", "resume an existing session by ID")
	stats := flag.Bool("stats", false, "show token usage statistics and exit")
	statsDays := flag.Int("stats-days", 0, "with -stats: show stats for the last N days (0 = all time)")
	flag.Parse()

	// Server mode: start the server and block until it shuts down.
	if *serve {
		if err := runServer(); err != nil {
			fmt.Fprintln(os.Stderr, "server error:", err)
			os.Exit(1)
		}
		return
	}

	// Client mode: ensure a server is running, then connect.
	if err := ensureServer(); err != nil {
		fmt.Fprintln(os.Stderr, "could not start server:", err)
		os.Exit(1)
	}

	// Stats mode: print the aggregated token usage report and exit. It
	// needs the server (for session data) but no model selection.
	if *stats {
		report, err := fetchStatsReport(*statsDays)
		if err != nil {
			fmt.Fprintln(os.Stderr, "could not fetch stats:", err)
			os.Exit(1)
		}
		fmt.Println(strings.Join(renderStats(report, 0, isTerminal(os.Stdout)), "\n"))
		return
	}

	// Resolve the model, provider, API key, and base URL.
	// With explicit -key/-url flags we build a single ad-hoc provider.
	// With just -model we find the provider that hosts it. With nothing
	// we default to the last used model, falling back to the model
	// selector when none was saved.
	providers := buildProviders()
	var selProvider provider
	var selModel string

	// No flags at all: default to the last used model if one was saved,
	// otherwise open the TUI with an empty model so its model selector
	// appears on startup.
	if *key == "" && *base == "" && *model == "" && *sessionID == "" {
		defaulted := false
		if lm, ok := loadLastModel(); ok {
			if p, found := providerByName(providers, lm.Provider); found {
				selProvider = p
				selModel = lm.Model
				defaulted = true
			}
		}
		if !defaulted {
			runTUI(providers, provider{}, "", SessionInfo{})
			return
		}
	}

	if *key != "" || *base != "" {
		// Explicit flags take priority: build a single ad-hoc provider.
		if *base == "" {
			if *key == "" {
				*base = "http://localhost:11434/v1"
			} else {
				*base = "https://ollama.com/v1"
			}
		}
		if *key == "" && strings.Contains(*base, "ollama.com") {
			fmt.Fprintln(os.Stderr, "no API key. Get one at https://ollama.com/settings/keys, then export OLLAMA_API_KEY, pass -key, or save it to ~/.local/share/atom/providers/ollama-cloud.")
			os.Exit(1)
		}
		if *model == "" {
			*model = "deepseek-v4-flash:cloud"
		}
		selProvider = provider{name: providerNameForURL(*base), baseURL: strings.TrimSuffix(*base, "/"), key: *key, reasoningField: reasoningFieldForURL(*base)}
		selModel = *model
	} else if *model != "" {
		// -model given without -key/-url: find the provider that hosts it.
		p, ok := findProviderForModel(providers, *model)
		if !ok {
			// Fall back to local Ollama.
			selProvider = provider{name: "ollama-local", baseURL: "http://localhost:11434/v1", key: "", reasoningField: "reasoning"}
		} else {
			selProvider = p
		}
		selModel = *model
	}

	// An explicitly chosen model becomes the new default for future
	// launches. Persistence failures are non-fatal.
	if selModel != "" {
		_ = saveLastModel(selProvider.name, selModel)
	}

	// Create or resume a session.
	var session SessionInfo
	if *sessionID != "" {
		var existing Session
		if err := apiGet("/api/sessions/"+*sessionID, &existing); err != nil {
			fmt.Fprintln(os.Stderr, "could not resume session:", err)
			os.Exit(1)
		}
		session = existing.info()
	} else {
		cwd, _ := os.Getwd()
		body, _ := json.Marshal(map[string]string{"model": selModel, "cwd": cwd})
		if err := apiPost("/api/sessions", body, &session); err != nil {
			fmt.Fprintln(os.Stderr, "could not create session:", err)
			os.Exit(1)
		}
	}

	runTUI(providers, selProvider, selModel, session)
}

// ensureServer checks whether the atom server is already running. If not,
// it starts one as a background process and waits for it to be ready.
func ensureServer() error {
	if serverRunning() {
		if serverSupportsCompaction() {
			return nil
		}
		// An older server is still bound to the socket and doesn't know
		// /compact. Recycle it so this client can use compaction.
		stopBackgroundServer()
	}

	// Start the server as a detached background process.
	exe, err := os.Executable()
	if err != nil {
		return fmt.Errorf("find executable: %w", err)
	}
	logFile := filepath.Join(dataDir(), "server.log")
	logF, err := os.OpenFile(logFile, os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0644)
	if err != nil {
		return fmt.Errorf("open log file: %w", err)
	}
	cmd := exec.Command(exe, "--serve")
	cmd.Stdout = logF
	cmd.Stderr = logF
	// Detach from the terminal so the server survives the client exiting.
	attr := syscallProcAttr()
	cmd.SysProcAttr = &attr
	if err := cmd.Start(); err != nil {
		return fmt.Errorf("start server: %w", err)
	}

	// Wait for the server to be ready (poll the socket for up to 5 seconds).
	for i := 0; i < 50; i++ {
		if serverRunning() {
			return nil
		}
		time.Sleep(100 * time.Millisecond)
	}
	return fmt.Errorf("server did not start within 5 seconds (see %s)", logFile)
}

// serverRunning reports whether the atom server is listening on its socket.
func serverRunning() bool {
	conn, err := net.Dial("unix", socketPath())
	if err != nil {
		return false
	}
	conn.Close()
	return true
}

// serverSupportsCompaction reports whether the live server exposes the
// compaction API. Older servers 404 on /api/capabilities.
func serverSupportsCompaction() bool {
	var caps struct {
		Compact bool `json:"compact"`
	}
	if err := apiGet("/api/capabilities", &caps); err != nil {
		return false
	}
	return caps.Compact
}

func stopBackgroundServer() {
	b, err := os.ReadFile(filepath.Join(dataDir(), "server.pid"))
	if err == nil {
		if pid, err := strconv.Atoi(strings.TrimSpace(string(b))); err == nil && pid > 0 {
			syscall.Kill(pid, syscall.SIGTERM)
		}
	}
	for i := 0; i < 50; i++ {
		if !serverRunning() {
			os.Remove(socketPath())
			os.Remove(filepath.Join(dataDir(), "server.pid"))
			return
		}
		time.Sleep(100 * time.Millisecond)
	}
}

// syscallProcAttr returns platform-specific process attributes to detach
// the server from the controlling terminal so it survives the client.
func syscallProcAttr() syscall.SysProcAttr {
	return syscall.SysProcAttr{Setsid: true}
}

// apiPost sends a JSON POST request to the server and decodes the response.
// If out is non-nil, the response body is JSON-decoded into it.
func apiPost(path string, body []byte, out interface{}) error {
	resp, err := httpPost(path, body)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode >= 400 {
		b, _ := io.ReadAll(io.LimitReader(resp.Body, 4096))
		return fmt.Errorf("%s: %s", resp.Status, strings.TrimSpace(string(b)))
	}
	if out != nil {
		return json.NewDecoder(resp.Body).Decode(out)
	}
	return nil
}

// apiPatch sends a JSON PATCH request to the server and decodes the response.
func apiPatch(path string, body []byte, out interface{}) error {
	resp, err := httpDo(http.MethodPatch, path, body)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode >= 400 {
		b, _ := io.ReadAll(io.LimitReader(resp.Body, 4096))
		return fmt.Errorf("%s: %s", resp.Status, strings.TrimSpace(string(b)))
	}
	if out != nil {
		return json.NewDecoder(resp.Body).Decode(out)
	}
	return nil
}

// apiGet sends a GET request to the server and decodes the JSON response.
func apiGet(path string, out interface{}) error {
	resp, err := httpDo(http.MethodGet, path, nil)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	if resp.StatusCode >= 400 {
		b, _ := io.ReadAll(io.LimitReader(resp.Body, 4096))
		return fmt.Errorf("%s: %s", resp.Status, strings.TrimSpace(string(b)))
	}
	return json.NewDecoder(resp.Body).Decode(out)
}

// httpPost is a helper that POSTs to the server's Unix socket.
func httpPost(path string, body []byte) (*http.Response, error) {
	return httpDo(http.MethodPost, path, body)
}

// httpDo sends a request to the server over its Unix socket.
func httpDo(method, path string, body []byte) (*http.Response, error) {
	var r io.Reader
	if body != nil {
		r = strings.NewReader(string(body))
	}
	req, err := http.NewRequest(method, "http://atom"+path, r)
	if err != nil {
		return nil, err
	}
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	client := &http.Client{
		Transport: &http.Transport{
			DialContext: func(_ context.Context, _, _ string) (net.Conn, error) {
				return net.Dial("unix", socketPath())
			},
		},
	}
	return client.Do(req)
}

// filterSessions returns sessions matching the query. An empty query
// returns all sessions. Matching is case-insensitive across the session
// ID, model, and title; every space-separated word must match somewhere,
// so multi-word searches like "ollama first" work.
func filterSessions(sessions []SessionInfo, query []byte) []SessionInfo {
	q := strings.ToLower(string(query))
	if q == "" {
		return sessions
	}
	words := strings.Fields(q)
	var out []SessionInfo
	for _, s := range sessions {
		if sessionMatchesQuery(words, strings.ToLower(s.ID), strings.ToLower(s.Model), strings.ToLower(s.Title)) {
			out = append(out, s)
		}
	}
	return out
}

// sessionMatchesQuery reports whether every word in words appears in the
// session's lowercase ID, model, or title.
func sessionMatchesQuery(words []string, id, model, title string) bool {
	for _, w := range words {
		if !strings.Contains(id, w) &&
			!strings.Contains(model, w) &&
			!strings.Contains(title, w) {
			return false
		}
	}
	return true
}

// thinkingLevels are the reasoning effort levels the user can cycle through
// with Tab. The names match OpenAI's reasoning_effort values, which
// Ollama's OpenAI-compatible endpoint passes through to the model.
var thinkingLevels = []string{"none", "low", "medium", "high", "max"}
var thinkingIdx = 4 // default: "max"

// formatTokens renders a token count compactly, like opencode does:
// "8.4K" above 1,000, "1.2M" above 1,000,000, and the raw number below.
func formatTokens(n int) string {
	switch {
	case n >= 1000000:
		return fmt.Sprintf("%.1fM", float64(n)/1000000)
	case n >= 1000:
		return fmt.Sprintf("%.1fK", float64(n)/1000)
	default:
		return strconv.Itoa(n)
	}
}

// contextWindowTokens returns the model's context window size in tokens.
// The model families atom commonly talks to (deepseek, qwen, and llama
// via Ollama Cloud and OpenCode Go) all use 128K windows, so unknown
// models fall back to that same size. The percentage shown next to the
// token count is only as good as this estimate.
func contextWindowTokens(model string) int {
	m := strings.ToLower(model)
	switch {
	case strings.Contains(m, "deepseek"),
		strings.Contains(m, "qwen"),
		strings.Contains(m, "llama"),
		strings.Contains(m, "gemma"),
		strings.Contains(m, "mistral"),
		strings.Contains(m, "ministral"),
		strings.Contains(m, "phi"):
		return 128000
	}
	return 128000
}

// sseData extracts the value of a data: SSE line, or returns ok=false.
func sseData(line string) (string, bool) {
	line = strings.TrimSpace(line)
	if !strings.HasPrefix(line, "data:") {
		return "", false
	}
	return strings.TrimSpace(strings.TrimPrefix(line, "data:")), true
}

// toolDefinitions declares the functions the model may call this session.
func toolDefinitions() []toolDef {
	searchParams := json.RawMessage(`{"type":"object","properties":{"query":{"type":"string","description":"The search query"}},"required":["query"]}`)
	readParams := json.RawMessage(`{"type":"object","properties":{"path":{"type":"string","description":"Absolute or relative path to the file to read"}},"required":["path"]}`)
	writeParams := json.RawMessage(`{"type":"object","properties":{"path":{"type":"string","description":"Absolute or relative path to the file to create or overwrite"},"content":{"type":"string","description":"The full content to write to the file"},"hash":{"type":"string","description":"The hash from the most recent read_file of this path. Required for existing files; omit when creating a new file."}},"required":["path","content"]}`)
	editParams := json.RawMessage(`{"type":"object","properties":{"path":{"type":"string","description":"Absolute or relative path to the file to edit"},"old_text":{"type":"string","description":"The exact text to find in the file"},"new_text":{"type":"string","description":"The replacement text"},"hash":{"type":"string","description":"The hash from the most recent read_file of this path"}},"required":["path","old_text","new_text","hash"]}`)
	bashParams := json.RawMessage(`{"type":"object","properties":{"command":{"type":"string","description":"The shell command to execute"}},"required":["command"]}`)

	tools := make([]toolDef, 5)
	tools[0].Type = "function"
	tools[0].Function.Name = "web_search"
	tools[0].Function.Description = "Search the web for current information. Returns search result titles, URLs, and snippets."
	tools[0].Function.Parameters = searchParams

	tools[1].Type = "function"
	tools[1].Function.Name = "read_file"
	tools[1].Function.Description = "Read the entire contents of a file. Returns the file content and a hash that must be passed back when writing or editing the file. Image files (png, jpg, gif, webp, bmp) are returned as images so vision-capable models can see them."
	tools[1].Function.Parameters = readParams

	tools[2].Type = "function"
	tools[2].Function.Name = "write_file"
	tools[2].Function.Description = "Create or overwrite a file with the given content. For existing files, pass the hash from read_file to detect stale references. Errors if the file changed since the last read."
	tools[2].Function.Parameters = writeParams

	tools[3].Type = "function"
	tools[3].Function.Name = "edit_file"
	tools[3].Function.Description = "Edit a file by replacing exact text. The hash must match the file's current content. Errors if the file changed since the last read or if old_text is not found."
	tools[3].Function.Parameters = editParams

	tools[4].Type = "function"
	tools[4].Function.Name = "bash"
	tools[4].Function.Description = "Execute a shell command and return its output. Use for running tests, checking git status, installing packages, etc."
	tools[4].Function.Parameters = bashParams

	return tools
}

// executeTool runs a named tool call with JSON-encoded arguments. It
// returns the result text to feed back to the model, any images attached
// to that result (read_file on an image file), plus a unified diff of
// any file changes ("" when the tool didn't change a file).
func executeTool(name, arguments, apiKey string) (string, []imageData, string) {
	switch name {
	case "web_search":
		var args struct {
			Query string `json:"query"`
		}
		if err := json.Unmarshal([]byte(arguments), &args); err != nil {
			return fmt.Sprintf("error parsing arguments: %v", err), nil, ""
		}
		return webSearch(args.Query, apiKey), nil, ""
	case "read_file":
		var args struct {
			Path string `json:"path"`
		}
		if err := json.Unmarshal([]byte(arguments), &args); err != nil {
			return fmt.Sprintf("error parsing arguments: %v", err), nil, ""
		}
		content, err := os.ReadFile(args.Path)
		if err != nil {
			return fmt.Sprintf("error reading file: %v", err), nil, ""
		}
		hash := sha256Hash(content)
		// Image files come back as images so the model can actually see
		// them; the text part names the file and its size.
		if mime := sniffImageMIME(content); mime != "" {
			if len(content) > maxImageSourceBytes {
				return fmt.Sprintf("error: image %s is %d bytes, larger than the %d-byte limit for reading images", args.Path, len(content), maxImageSourceBytes), nil, ""
			}
			out, outMIME, err := normalizeImage(content)
			if err != nil {
				return fmt.Sprintf("error: cannot attach image %s: %v", args.Path, err), nil, ""
			}
			img := imageData{MIME: outMIME, Data: base64.StdEncoding.EncodeToString(out)}
			return fmt.Sprintf("hash: %s\nImage file: %s (%d bytes)", hash, args.Path, len(out)), []imageData{img}, ""
		}
		return fmt.Sprintf("hash: %s\n%s", hash, string(content)), nil, ""
	case "write_file":
		var args struct {
			Path    string `json:"path"`
			Content string `json:"content"`
			Hash    string `json:"hash"`
		}
		if err := json.Unmarshal([]byte(arguments), &args); err != nil {
			return fmt.Sprintf("error parsing arguments: %v", err), nil, ""
		}
		// For existing files, verify the hash matches (staleness check).
		var existing []byte
		if e, err := os.ReadFile(args.Path); err == nil {
			existing = e
			if args.Hash == "" {
				return "error: file already exists. Read it first with read_file to get its hash, then pass the hash to write_file.", nil, ""
			}
			currentHash := sha256Hash(existing)
			if args.Hash != currentHash {
				return fmt.Sprintf("error: file has changed since last read (expected hash %s, got %s). Read the file again with read_file before writing.", args.Hash, currentHash), nil, ""
			}
		}
		if err := os.WriteFile(args.Path, []byte(args.Content), 0644); err != nil {
			return fmt.Sprintf("error writing file: %v", err), nil, ""
		}
		return fmt.Sprintf("wrote %d bytes to %s", len(args.Content), args.Path),
			nil, fileDiff(args.Path, existing, []byte(args.Content))
	case "edit_file":
		var args struct {
			Path    string `json:"path"`
			OldText string `json:"old_text"`
			NewText string `json:"new_text"`
			Hash    string `json:"hash"`
		}
		if err := json.Unmarshal([]byte(arguments), &args); err != nil {
			return fmt.Sprintf("error parsing arguments: %v", err), nil, ""
		}
		content, err := os.ReadFile(args.Path)
		if err != nil {
			return fmt.Sprintf("error reading file: %v", err), nil, ""
		}
		// Verify the hash matches (staleness check).
		currentHash := sha256Hash(content)
		if args.Hash != currentHash {
			return fmt.Sprintf("error: file has changed since last read (expected hash %s, got %s). Read the file again with read_file before editing.", args.Hash, currentHash), nil, ""
		}
		// Find and replace the old text. Error if not found.
		count := strings.Count(string(content), args.OldText)
		if count == 0 {
			return "error: old_text not found in file.", nil, ""
		}
		if count > 1 {
			return fmt.Sprintf("error: old_text found %d times in file. It must be unique. Include more surrounding context.", count), nil, ""
		}
		updated := strings.Replace(string(content), args.OldText, args.NewText, 1)
		if err := os.WriteFile(args.Path, []byte(updated), 0644); err != nil {
			return fmt.Sprintf("error writing file: %v", err), nil, ""
		}
		return fmt.Sprintf("edited %s: replaced %d bytes with %d bytes", args.Path, len(args.OldText), len(args.NewText)),
			nil, fileDiff(args.Path, content, []byte(updated))
	case "bash":
		var args struct {
			Command string `json:"command"`
		}
		if err := json.Unmarshal([]byte(arguments), &args); err != nil {
			return fmt.Sprintf("error parsing arguments: %v", err), nil, ""
		}
		cmd := exec.Command("bash", "-c", args.Command)
		cmd.Dir, _ = os.Getwd()
		out, err := cmd.CombinedOutput()
		if err != nil {
			return fmt.Sprintf("exit %v\n%s", err, string(out)), nil, ""
		}
		return strings.TrimSpace(string(out)), nil, ""
	default:
		return fmt.Sprintf("unknown tool: %s", name), nil, ""
	}
}

// sniffImageMIME detects the MIME type of an image file from its magic
// bytes. Returns "" when the data isn't a recognizable image.
func sniffImageMIME(data []byte) string {
	switch {
	case len(data) >= 8 && bytes.Equal(data[:8], []byte{0x89, 'P', 'N', 'G', 0x0D, 0x0A, 0x1A, 0x0A}):
		return "image/png"
	case len(data) >= 3 && bytes.Equal(data[:3], []byte{0xFF, 0xD8, 0xFF}):
		return "image/jpeg"
	case len(data) >= 6 && (bytes.Equal(data[:6], []byte("GIF87a")) || bytes.Equal(data[:6], []byte("GIF89a"))):
		return "image/gif"
	case len(data) >= 12 && bytes.Equal(data[:4], []byte("RIFF")) && bytes.Equal(data[8:12], []byte("WEBP")):
		return "image/webp"
	case len(data) >= 2 && bytes.Equal(data[:2], []byte{'B', 'M'}):
		return "image/bmp"
	}
	return ""
}

// modelSupportsImages reports whether a model name looks vision-capable,
// based on the vision model families atom commonly talks to. It's a
// best-effort heuristic — providers don't expose a standard capability
// API — so it only drives a hint in the TUI, never the request itself.
func modelSupportsImages(model string) bool {
	m := strings.ToLower(model)
	names := []string{
		"llava", "vision", "minicpm-v", "moondream", "bakllava",
		"qwen-vl", "qwen2-vl", "qwen2.5-vl", "qwen3-vl", "deepseek-vl",
		"gemma3", "phi-4-vision", "internvl", "glm-4v",
		"gpt-4o", "gpt-4.1", "gpt-4v", "gpt-4-vision", "o4-mini",
		"claude", "gemini", "kimi", "minimax",
	}
	for _, n := range names {
		if strings.Contains(m, n) {
			return true
		}
	}
	return false
}

// fileDiff returns a unified diff between old and new file content, or ""
// when the content is unchanged. The diff headers carry the file path so
// the client can label the change.
func fileDiff(path string, old, new []byte) string {
	if bytes.Equal(old, new) {
		return ""
	}
	return udiff.Unified(path, path, string(old), string(new))
}

// sha256Hash returns the hex-encoded SHA-256 hash of data.
func sha256Hash(data []byte) string {
	h := sha256.Sum256(data)
	return hex.EncodeToString(h[:])
}

// webSearch queries the web through Ollama Cloud's search endpoint using the
// same API key used for chat. Returns results as plain text for the model.
func webSearch(query, apiKey string) string {
	body, err := json.Marshal(struct {
		Query      string `json:"query"`
		MaxResults int    `json:"max_results"`
	}{Query: query, MaxResults: 5})
	if err != nil {
		return fmt.Sprintf("search error: %v", err)
	}

	req, err := http.NewRequest(http.MethodPost, "https://ollama.com/api/web_search", strings.NewReader(string(body)))
	if err != nil {
		return fmt.Sprintf("search error: %v", err)
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Authorization", "Bearer "+apiKey)

	client := &http.Client{Timeout: 30 * time.Second}
	resp, err := client.Do(req)
	if err != nil {
		return fmt.Sprintf("search error: %v", err)
	}
	defer resp.Body.Close()

	var data struct {
		Results []struct {
			Title   string `json:"title"`
			URL     string `json:"url"`
			Content string `json:"content"`
		} `json:"results"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&data); err != nil {
		return fmt.Sprintf("search error: %v", err)
	}

	var sb strings.Builder
	for i, r := range data.Results {
		fmt.Fprintf(&sb, "%d. %s\n   %s\n   %s\n\n", i+1, r.Title, r.URL, r.Content)
	}
	if sb.Len() == 0 {
		return "no results found"
	}
	return strings.TrimSpace(sb.String())
}
