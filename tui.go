// tui.go is the Bubble Tea interface for atom. It renders a scrolling
// conversation view with a fixed input line at the bottom, and overlay
// selectors for switching models and sessions.
package main

import (
	"bufio"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"strconv"
	"strings"
	"time"

	tea "charm.land/bubbletea/v2"
	"charm.land/bubbles/v2/key"
	"charm.land/bubbles/v2/spinner"
	"charm.land/bubbles/v2/textarea"
	"charm.land/bubbles/v2/viewport"
	"charm.land/lipgloss/v2"
	"github.com/charmbracelet/x/ansi"
	uv "github.com/charmbracelet/ultraviolet"
)

// --- styles ---

var (
	styleUser      = lipgloss.NewStyle().Foreground(lipgloss.Color("6"))
	styleAssistant = lipgloss.NewStyle().Foreground(lipgloss.Color("7"))
	styleReasoning = lipgloss.NewStyle().Foreground(lipgloss.Color("8"))
	styleTool      = lipgloss.NewStyle().Foreground(lipgloss.Color("252")).Background(lipgloss.Color("237"))
	styleError     = lipgloss.NewStyle().Foreground(lipgloss.Color("1"))
	stylePrompt    = lipgloss.NewStyle().Foreground(lipgloss.Color("4"))
	styleCursor    = lipgloss.NewStyle().Foreground(lipgloss.Color("4")).Bold(true)
	styleDim       = lipgloss.NewStyle().Foreground(lipgloss.Color("8"))
	styleSelected  = lipgloss.NewStyle().Bold(true).Foreground(lipgloss.Color("4"))
	styleDiffAdd   = lipgloss.NewStyle().Foreground(lipgloss.Color("2")) // green: added lines
	styleDiffDel   = lipgloss.NewStyle().Foreground(lipgloss.Color("1")) // red: deleted lines
	styleDiffHdr   = lipgloss.NewStyle().Foreground(lipgloss.Color("8")) // dim: hunk headers
	// Prompt rules: saturated blue at low contrast (true alpha isn't
	// available on box-drawing text, so this is ~20% of #3a8fd4 on a
	// typical dark background).
	stylePromptBorder = lipgloss.NewStyle().Foreground(lipgloss.Color("#213443"))
	styleImgChip      = lipgloss.NewStyle().Foreground(lipgloss.Color("0")).Background(lipgloss.Color("12")).Padding(0, 1)
)

// tuiHPad is the left/right inset in cells. One cell is ~8px in a
// typical terminal, so this is the closest match to ~4px each side.
const tuiHPad = 1

// --- block types ---

// block is one rendered unit in the conversation: a user message, an
// assistant reply, reasoning text, a compaction phase, a tool call, or an error.
type block struct {
	kind string // "user", "assistant", "reasoning", "compaction", "tool", "error"
	text string
	diff string // unified diff of the file a tool edited, "" when none
	// active marks a reasoning block that is still streaming;
	// startedAt and dur back the collapsed "Thinking (8.3s)" line
	// shown while reasoning display is off (see /thinking).
	active    bool
	startedAt time.Time
	dur       time.Duration

	// Render cache: wrapped lines at lineWidth. lines == nil is a miss.
	lines     []string
	lineWidth int
	lineShowR bool
}

// --- tea model ---

type tuiModel struct {
	// session and provider state
	providers   []provider
	selProvider provider
	selModel    string
	session     SessionInfo

	// thinking level
	thinkingIdx int

	// whether reasoning blocks are rendered in the conversation view;
	// toggled with /thinking. The blocks stay in the history either way.
	showReasoning bool

	// conversation
	blocks   []block
	viewport viewport.Model
	input    textarea.Model
	lastInH  int // input height from the last layout, to detect growth
	// contentLines is the viewport's wrapped line slice, concatenated
	// from per-block caches. blockStart[i] is the first line of blocks[i].
	contentLines []string
	blockStart   []int
	contentWidth int
	// following pins the viewport to the newest output. It stays true
	// until the user scrolls up; while false, streamed content leaves
	// the scroll position alone so scrollback stays readable mid-stream.
	following bool

	// cursor blink state, toggled by blinkMsg every half second. It
	// drives the prompt cursor drawn when the input is empty (the
	// textarea hides its cursor then) and the overlay search cursors.
	blinkOn bool

	// SSE subscription for real-time updates from other instances
	eventSub   chan streamMsg
	eventSubID string // session the subscription belongs to

	// streaming state
	streaming  bool
	streamSub  chan streamMsg // active stream channel during streaming
	turnID     string         // ID of the active turn, so Esc can pause it
	paused     bool           // the last stream was paused (Esc or no viewers)
	workingMsg string         // "loading models", "loading sessions", etc.
	spinner    spinner.Model  // animated "thinking" indicator in the status bar

	// overlay state: "" = none, "model" = model selector, "session" =
	// session picker, "stats" = token usage report
	overlay         string
	overlayQ        []byte // search query for the overlay
	overlaySel      int    // selected index (also the stats scroll offset)
	overlayEntries  []modelEntry
	overlaySessions []SessionInfo
	overlayStats    *statsReport // fetched report for the /stats overlay
	statsDays       int          // /stats N window in days, 0 = all time

	// terminal dimensions
	width  int
	height int

	// transient error message
	errMsg string

	// slash-command menu: visible when the user types "/" and we have matches
	menuVisible bool
	menuSel     int // selected index in the menu

	// pasted images attached to the pending prompt. Each shows a small
	// preview above the input with its marker number; the marker ([n])
	// sits in the input at the insertion point. previewDirty flags that
	// the pending set changed and kitty virtual placements need a
	// transmit or delete.
	pending      []pendingImage
	previewDirty bool
	kittyChunks map[int][]byte // partial kitty graphic transmissions by id

	// whether the program should quit
	quitting bool
}

// pendingImage is one pasted image waiting to be sent: the attachment
// itself plus its preview layout. cols/rows is the preview's cell box
// (at most 16x6); cols=0 marks an image that couldn't be decoded, which
// renders as a text row instead.
type pendingImage struct {
	img  imageData
	name string
	cols int
	rows int
}

// command is a slash command the user can type in the chat.
type command struct {
	name string
	desc string
}

var commands = []command{
	{"/model", "switch model"},
	{"/new", "start a new session"},
	{"/sessions", "list all sessions"},
	{"/stats", "show token usage stats"},
	{"/compact", "summarize conversation context"},
	{"/thinking", "toggle reasoning display"},
	{"/quit", "exit"},
}

// matchCommands returns commands whose name starts with prefix.
func matchCommands(prefix string) []command {
	var out []command
	for _, c := range commands {
		if strings.HasPrefix(c.name, prefix) {
			out = append(out, c)
		}
	}
	return out
}

// --- messages ---

// streamMsg carries one NDJSON event from the server's /send endpoint.
type streamMsg struct {
	eventType string
	text      string
	name      string
	arguments string
	message   string
	diff      string
	err       error
	usage     *streamUsage // set for "usage" events
}

// compactDoneMsg is the result of a /compact request.
type compactDoneMsg struct {
	err error
}

// streamStartMsg carries the channel that a background goroutine pumps
// stream events into. It's returned by sendCmd; Update stores the channel
// and chains waitForStreamCmd to receive events one at a time.
type streamStartMsg struct {
	sub chan streamMsg
}

// streamDoneMsg signals the server finished responding.
type streamDoneMsg struct{}

// modelsLoadedMsg carries the full list of model entries for the selector.
type modelsLoadedMsg struct {
	entries []modelEntry
}

// sessionsLoadedMsg carries all sessions for the session picker.
type sessionsLoadedMsg struct {
	sessions []SessionInfo
}

// errMsg carries an error to display.
type errorMSG struct{ err error }

// blinkMsg toggles the TUI's cursor blink state on a half-second timer.
type blinkMsg struct{}

// blinkCmd returns a command that fires blinkMsg after 530ms. The TUI
// re-arms it on every blinkMsg so the cursors keep blinking.
func blinkCmd() tea.Cmd {
	return tea.Tick(530*time.Millisecond, func(time.Time) tea.Msg { return blinkMsg{} })
}

// --- commands ---

// newTurnID generates a unique ID for a send so a pause request can
// target the right turn even when it races ahead of the send.
func newTurnID() string {
	return fmt.Sprintf("%d", time.Now().UnixNano())
}

// pauseCmd asks the server to stop the active stream for a session. The
// server cancels the in-flight model request, so generation stops
// immediately; the /send stream then closes and the TUI finishes up.
func pauseCmd(sessionID, turnID string) tea.Cmd {
	return func() tea.Msg {
		body, _ := json.Marshal(map[string]string{"turn_id": turnID})
		if err := apiPost("/api/sessions/"+sessionID+"/pause", body, nil); err != nil {
			return errorMSG{err: err}
		}
		return nil
	}
}

// compactCmd asks an in-flight turn to pause generation, fold history,
// and resume. Idle /compact uses sendCmd instead so the TUI can render
// compaction events on the same NDJSON stream as chat.
func compactCmd(sessionID, extra string) tea.Cmd {
	return func() tea.Msg {
		body, _ := json.Marshal(map[string]string{"instructions": extra})
		if err := apiPost("/api/sessions/"+sessionID+"/compact", body, nil); err != nil {
			return compactDoneMsg{err: err}
		}
		return compactDoneMsg{}
	}
}

// sendCmd posts a message to the server and reads the NDJSON stream,
// emitting one streamMsg per event. Each event is delivered as a separate
// tea.Msg so the UI updates incrementally as chunks arrive. images are
// the attachments pasted into the prompt.
func sendCmd(sessionID, turnID, prompt string, images []imageData, key, baseURL, thinking, reasoningField string, compact bool, compactInstructions string) tea.Cmd {
	return func() tea.Msg {
		body, _ := json.Marshal(struct {
			Message             string      `json:"message"`
			Thinking            string      `json:"thinking"`
			Key                 string      `json:"key"`
			BaseURL             string      `json:"base_url"`
			ReasoningField      string      `json:"reasoning_field"`
			TurnID              string      `json:"turn_id"`
			Images              []imageData `json:"images"`
			Compact             bool        `json:"compact"`
			CompactInstructions string      `json:"compact_instructions"`
		}{prompt, thinking, key, baseURL, reasoningField, turnID, images, compact, compactInstructions})
		resp, err := httpPost("/api/sessions/"+sessionID+"/send", body)
		if err != nil {
			// The server may have shut down, or its socket file may have
			// been taken from under it by a racing instance. Restart the
			// server and retry the send once before giving up.
			if !serverRunning() {
				if ensureErr := ensureServer(); ensureErr == nil {
					resp, err = httpPost("/api/sessions/"+sessionID+"/send", body)
				}
			}
			if err != nil {
				return streamMsg{err: err}
			}
		}
		if resp.StatusCode >= 400 {
			b, _ := io.ReadAll(io.LimitReader(resp.Body, 4096))
			resp.Body.Close()
			return streamMsg{err: fmt.Errorf("%s: %s", resp.Status, strings.TrimSpace(string(b)))}
		}

		// Pump NDJSON events into a channel from a background goroutine.
		// Each event is received by waitForStreamCmd, which chains itself
		// so the UI updates incrementally as chunks arrive.
		sub := make(chan streamMsg, 64)
		go func() {
			defer resp.Body.Close()
			reader := bufio.NewReader(resp.Body)
			for {
				line, err := reader.ReadString('\n')
				if line != "" {
					sub <- parseStreamLine(line)
				}
				if err != nil {
					if err != io.EOF && line == "" {
						sub <- streamMsg{err: err}
					}
					close(sub)
					return
				}
			}
		}()

		return streamStartMsg{sub: sub}
	}
}

// waitForStreamCmd waits for the next event on the stream channel and
// returns it as a streamMsg (or streamDoneMsg when the channel closes).
func waitForStreamCmd(sub chan streamMsg) tea.Cmd {
	return func() tea.Msg {
		msg, ok := <-sub
		if !ok {
			return streamDoneMsg{}
		}
		return msg
	}
}

// parseStreamLine decodes one NDJSON line into a streamMsg.
func parseStreamLine(line string) streamMsg {
	var ev map[string]string
	if json.Unmarshal([]byte(strings.TrimSpace(line)), &ev) != nil {
		return streamMsg{}
	}
	msg := streamMsg{
		eventType: ev["type"],
		text:      ev["text"],
		name:      ev["name"],
		arguments: ev["arguments"],
		message:   ev["message"],
		diff:      ev["diff"],
	}
	// "usage" events carry the provider-reported token counts.
	if ev["total"] != "" {
		prompt, _ := strconv.Atoi(ev["prompt"])
		completion, _ := strconv.Atoi(ev["completion"])
		total, _ := strconv.Atoi(ev["total"])
		if total > 0 {
			msg.usage = &streamUsage{
				PromptTokens:     prompt,
				CompletionTokens: completion,
				TotalTokens:      total,
			}
		}
	}
	return msg
}

// fetchModelsCmd fetches models from all providers concurrently and
// returns a merged, sorted list.
func fetchModelsCmd(providers []provider) tea.Cmd {
	return func() tea.Msg {
		return modelsLoadedMsg{entries: fetchAllModels(providers)}
	}
}

// fetchAllModels fetches models from all providers and returns a sorted
// list of modelEntry values.
func fetchAllModels(providers []provider) []modelEntry {
	type result struct {
		provider provider
		models   []string
	}
	results := make(chan result, len(providers))
	for _, p := range providers {
		go func(p provider) {
			models, err := fetchModels(p)
			if err != nil {
				results <- result{provider: p}
				return
			}
			results <- result{provider: p, models: models}
		}(p)
	}

	var entries []modelEntry
	for range providers {
		r := <-results
		for _, m := range r.models {
			entries = append(entries, modelEntry{provider: r.provider, model: m})
		}
	}

	// Sort by provider name then model name.
	for i := 1; i < len(entries); i++ {
		for j := i; j > 0; j-- {
			a, b := entries[j-1], entries[j]
			if a.provider.name > b.provider.name ||
				(a.provider.name == b.provider.name && a.model > b.model) {
				entries[j-1], entries[j] = entries[j], entries[j-1]
			} else {
				break
			}
		}
	}
	return entries
}

// listSessionsCmd fetches all sessions from the server.
func listSessionsCmd() tea.Cmd {
	return func() tea.Msg {
		var sessions []SessionInfo
		if err := apiGet("/api/sessions", &sessions); err != nil {
			return errorMSG{err: err}
		}
		return sessionsLoadedMsg{sessions: sessions}
	}
}

// statsLoadedMsg carries the aggregated token usage report for the
// /stats overlay.
type statsLoadedMsg struct {
	report statsReport
}

// fetchStatsCmd fetches the aggregated token usage report from the
// server's /api/stats endpoint. days > 0 restricts the report to the
// last N days; 0 means all time.
func fetchStatsCmd(days int) tea.Cmd {
	return func() tea.Msg {
		report, err := fetchStatsReport(days)
		if err != nil {
			return errorMSG{err: err}
		}
		return statsLoadedMsg{report: report}
	}
}

// --- model implementation ---

func initialModel(providers []provider, selProvider provider, selModel string, session SessionInfo) tuiModel {
	vp := viewport.New(viewport.WithWidth(80), viewport.WithHeight(20))
	vp.SetContent("")

	ta := textarea.New()
	ta.Placeholder = ""
	ta.CharLimit = 0
	ta.ShowLineNumbers = false
	// Enter sends the message (handled by the TUI); Shift+Enter, Alt+Enter
	// and Ctrl+J insert a newline so a prompt can span multiple lines.
	// Bubble Tea v2 enables keyboard disambiguation on supporting
	// terminals, so Shift+Enter arrives as its own key event.
	ta.KeyMap.InsertNewline = key.NewBinding(
		key.WithKeys("alt+enter", "ctrl+j", "shift+enter"),
		key.WithHelp("alt+enter", "insert newline"),
	)
	// Cmd+A selects the prompt only (not the conversation). Super is
	// the Command key on macOS; the default binding is ctrl+g.
	ta.KeyMap.SelectAll = key.NewBinding(
		key.WithKeys("super+a"),
		key.WithHelp("cmd+a", "select prompt"),
	)
	// The textarea's defaults draw a prompt glyph ("▏") before every line
	// and a background behind the cursor line. Strip both so the prompt
	// keeps the plain look of the old textinput field. The textarea's own
	// virtual cursor is the single cursor: make it static (no blink) and
	// color it blue to match the app accent.
	ta.Prompt = ""
	s := ta.Styles()
	s.Focused.CursorLine = lipgloss.NewStyle()
	s.Blurred.CursorLine = lipgloss.NewStyle()
	s.Cursor.Blink = false
	s.Cursor.Color = lipgloss.Color("4")
	ta.SetStyles(s)
	ta.SetWidth(77)
	ta.SetHeight(1)
	ta.Focus()

	// The thinking spinner replaces the old "thinking..." text in the
	// status bar while a turn is streaming. MiniDot is the braille
	// spinner preset; color it with the app's blue accent.
	sp := spinner.New()
	sp.Spinner = spinner.MiniDot
	sp.Style = lipgloss.NewStyle().Foreground(lipgloss.Color("4"))

	m := tuiModel{
		providers:   providers,
		selProvider: selProvider,
		selModel:    selModel,
		session:     session,
		thinkingIdx: 4, // default: "max"
		viewport:    vp,
		input:       ta,
		spinner:     sp,
		blinkOn:     true,
		following:   true, // start pinned to the newest output
	}
	m.showReasoning = true // reasoning blocks are visible by default

	// If no model was selected, auto-open the model selector on startup.
	if selModel == "" && session.ID == "" {
		m.overlay = "model"
		m.workingMsg = "loading models..."
	}

	return m
}

// messagesToBlocks converts a session's message history into TUI blocks
// for display. User messages become "user" blocks, assistant messages
// become "reasoning" (if present) then "assistant" blocks, and tool calls
// become "tool" blocks.
// sessionToBlocks renders the transcript, then the compaction brief if
// the session was folded but never stored a display copy (older saves).
func sessionToBlocks(sess Session) []block {
	blocks := messagesToBlocks(sess.Messages)
	if sess.CompactionSummary == "" {
		return blocks
	}
	for _, b := range blocks {
		if b.kind == "compaction" && b.text != "" {
			return blocks
		}
	}
	return append(blocks, block{
		kind: "compaction",
		text: compactionPromptText(sess.CompactionSummary),
	})
}

func messagesToBlocks(msgs []message) []block {
	var blocks []block
	for _, msg := range msgs {
		switch msg.Role {
		case "user":
			blocks = append(blocks, block{kind: "user", text: msg.Content})
		case "compaction":
			blocks = append(blocks, block{kind: "compaction", text: msg.Content})
		case "assistant":
			if msg.Reasoning != "" {
				blocks = append(blocks, block{kind: "reasoning", text: msg.Reasoning})
			}
			if msg.Content != "" {
				blocks = append(blocks, block{kind: "assistant", text: msg.Content})
			}
			for _, tc := range msg.ToolCalls {
				blocks = append(blocks, block{kind: "tool", text: tc.Function.Name + ": " + tc.Function.Arguments})
			}
		case "tool":
			// Tool result messages show the outcome plus the diff of any
			// file edits the tool made.
			blocks = append(blocks, block{kind: "tool", text: msg.Content, diff: msg.Diff})
		}
	}
	return blocks
}

// restoreReasoningDurations copies measured reasoning durations from the
// previous blocks onto reloaded blocks with matching reasoning text. A
// session reload rebuilds blocks from persisted history, which stores
// the reasoning text but not how long it took; without this the
// collapsed "Thinking (8.3s)" line would lose its duration right after a
// turn completes (the server's "saved" event triggers such a reload).
// Matching by text keeps the copy safe across session switches, where
// the old and new conversations share no reasoning.
func restoreReasoningDurations(blocks, prev []block) {
	durs := make(map[string]time.Duration)
	for _, b := range prev {
		if b.kind == "reasoning" && b.dur > 0 {
			durs[b.text] = b.dur
		}
	}
	for i := range blocks {
		if blocks[i].kind == "reasoning" && blocks[i].dur == 0 {
			if d, ok := durs[blocks[i].text]; ok {
				blocks[i].dur = d
			}
		}
	}
}

// loadSessionCmd fetches the full session (with messages) from the server
// and returns a sessionLoadedMsg so the TUI can populate the conversation.
func loadSessionCmd(sessionID string) tea.Cmd {
	return func() tea.Msg {
		var sess Session
		if err := apiGet("/api/sessions/"+sessionID, &sess); err != nil {
			return errorMSG{err: err}
		}
		return sessionLoadedMsg{session: sess}
	}
}

// sessionLoadedMsg carries a full session (with message history) to display.
type sessionLoadedMsg struct {
	session Session
}

// subscribeCmd opens an SSE connection to the server's /events endpoint
// and pumps NDJSON events into a channel. The TUI chains waitForEventCmd
// to receive them one at a time, giving real-time updates when another
// atom instance sends a message to the same session. If the server is
// unreachable (e.g. it idle-shut-down), it restarts the server and
// returns subEndedMsg so the TUI retries with a delay.
func subscribeCmd(sessionID string) tea.Cmd {
	return func() tea.Msg {
		resp, err := httpDo(http.MethodGet, "/api/sessions/"+sessionID+"/events", nil)
		if err != nil {
			// The server may have idle-shut-down. Restart it so the
			// retry (subEndedMsg path) can succeed.
			if !serverRunning() {
				if ensureErr := ensureServer(); ensureErr != nil {
					return subEndedMsg{sessionID: sessionID, retryAfter: 3 * time.Second}
				}
			}
			return subEndedMsg{sessionID: sessionID, retryAfter: time.Second}
		}
		if resp.StatusCode >= 400 {
			b, _ := io.ReadAll(io.LimitReader(resp.Body, 4096))
			resp.Body.Close()
			return errorMSG{err: fmt.Errorf("%s: %s", resp.Status, strings.TrimSpace(string(b)))}
		}

		sub := make(chan streamMsg, 64)
		go func() {
			defer resp.Body.Close()
			reader := bufio.NewReader(resp.Body)
			for {
				line, err := reader.ReadString('\n')
				if line != "" {
					sub <- parseStreamLine(line)
				}
				if err != nil {
					close(sub)
					return
				}
			}
		}()

		return subStartedMsg{sub: sub, sessionID: sessionID}
	}
}

// subStartedMsg carries the event channel for the SSE subscription.
type subStartedMsg struct {
	sub       chan streamMsg
	sessionID string
}

// waitForEventCmd waits for the next SSE event from the subscription
// channel. When the channel closes (server disconnected), it returns
// subEndedMsg so the TUI can reconnect. The channel is captured in the
// closure so a chain always drains its own subscription, even after the
// model has switched to a different session.
func waitForEventCmd(sub chan streamMsg, sessionID string) tea.Cmd {
	return func() tea.Msg {
		msg, ok := <-sub
		if !ok {
			return subEndedMsg{sessionID: sessionID}
		}
		return eventMsg{streamMsg: msg, sessionID: sessionID, sub: sub}
	}
}

// eventMsg wraps a streamMsg that came from the SSE subscription (another
// instance's activity), distinct from our own streamMsg so Update can
// route them separately. sessionID and sub identify the subscription the
// event arrived on, letting the TUI drop events from a stale subscription
// after switching sessions without breaking the live chain.
type eventMsg struct {
	streamMsg
	sessionID string
	sub       chan streamMsg
}

// subEndedMsg signals the SSE subscription disconnected and needs
// reconnect. retryAfter spaces out reconnect attempts so we don't spin
// when the server is down.
type subEndedMsg struct {
	sessionID  string
	retryAfter time.Duration
}

// delayedCmd runs cmd after a delay. tea.Cmd functions run on their own
// goroutine, so sleeping here doesn't block the UI.
func delayedCmd(d time.Duration, cmd tea.Cmd) tea.Cmd {
	if d <= 0 {
		return cmd
	}
	return func() tea.Msg {
		time.Sleep(d)
		return cmd()
	}
}

func (m tuiModel) Init() tea.Cmd {
	// The blink timer runs continuously so the prompt and overlay search
	// cursors keep blinking; batch it with the startup work below.
	cmds := []tea.Cmd{blinkCmd()}
	if m.overlay == "model" {
		return tea.Batch(append(cmds, fetchModelsCmd(m.providers))...)
	}
	// If we have a session with an ID, load its message history.
	if m.session.ID != "" {
		return tea.Batch(append(cmds, loadSessionCmd(m.session.ID))...)
	}
	return tea.Batch(cmds...)
}

func (m *tuiModel) Update(msg tea.Msg) (tea.Model, tea.Cmd) {
	// Clipboard reads run as a Cmd; apply the result even if an overlay
	// opened while osascript/wl-paste was in flight.
	if clip, ok := msg.(clipboardPasteMsg); ok {
		if m.overlay != "" {
			return m, nil
		}
		return m.applyClipboardPaste(clip)
	}

	// Advance the thinking spinner while a turn is streaming. Handle it
	// before the overlay branch so the tick chain doesn't break when an
	// overlay opens mid-stream; once streaming ends the chain stops.
	if tick, ok := msg.(spinner.TickMsg); ok {
		var cmd tea.Cmd
		m.spinner, cmd = m.spinner.Update(tick)
		if !m.streaming {
			return m, nil
		}
		return m, cmd
	}

	// Handle overlay mode separately — it captures all keys.
	if m.overlay != "" {
		return m.updateOverlay(msg)
	}

	switch msg := msg.(type) {
	case tea.WindowSizeMsg:
		prevW := m.width
		m.width = msg.Width
		m.height = msg.Height
		m.layoutViewport()
		if msg.Width != prevW {
			m.refreshViewport()
		}
		return m, nil

	case blinkMsg:
		m.blinkOn = !m.blinkOn
		return m, blinkCmd()

	case previewPaintedMsg:
		// The preview repaint finished; nothing else to do.
		return m, nil

	case uv.UnknownOscEvent:
		// OSC 1337 is an image paste (iTerm2, WezTerm).
		if name, data, ok := parseOSC1337(string(msg)); ok {
			if err := m.pasteImage(name, data); err != nil {
				m.errMsg = err.Error()
			}
			return m, m.previewCmd()
		}
		return m, nil

	case uv.KittyGraphicsEvent:
		return m.handleKittyGraphics(msg)

	case tea.PasteMsg:
		// A paste mixing text and OSC 1337 images (some terminals wrap
		// image pastes in bracketed paste) inserts both in order.
		if strings.Contains(msg.Content, "\x1b]1337;") {
			return m.pasteMixedContent(msg.Content)
		}
		// Finder / kitty file drops arrive as quoted paths.
		if files := localImagesFromPaste(msg.Content); len(files) > 0 {
			return m.pasteLocalImages(files)
		}
		var cmd tea.Cmd
		m.input, cmd = m.input.Update(msg)
		m.layoutViewport()
		return m, cmd

	case tea.KeyPressMsg:
		// Alt+Enter and Shift+Enter insert a newline into the prompt
		// instead of sending. Bubble Tea v2 enables keyboard
		// disambiguation on supporting terminals, so Shift+Enter arrives
		// as its own key event; on terminals without support it's
		// indistinguishable from Enter (which sends).
		switch msg.String() {
		case "alt+enter", "shift+enter":
			var cmd tea.Cmd
			m.input, cmd = m.input.Update(msg)
			m.layoutViewport()
			return m, cmd
		}

		// If the slash-command menu is visible, handle navigation keys.
		if m.menuVisible {
			switch msg.String() {
			case "esc":
				m.setMenuVisible(false)
				m.layoutViewport()
				return m, nil
			case "up":
				if m.menuSel > 0 {
					m.menuSel--
				}
				return m, nil
			case "down":
				matches := matchCommands(m.input.Value())
				if m.menuSel < len(matches)-1 {
					m.menuSel++
				}
				return m, nil
			case "enter":
				matches := matchCommands(m.input.Value())
				if len(matches) > 0 && m.menuSel < len(matches) {
					m.input.SetValue(matches[m.menuSel].name)
				}
				m.setMenuVisible(false)
				m.layoutViewport()
				text := strings.TrimSpace(m.input.Value())
				if text == "" {
					return m, nil
				}
				return m.handleInput(text)
			case "tab":
				// Tab on the menu: complete to the selected command.
				matches := matchCommands(m.input.Value())
				if len(matches) > 0 && m.menuSel < len(matches) {
					m.input.SetValue(matches[m.menuSel].name)
					m.setMenuVisible(false)
					m.layoutViewport()
				}
				return m, nil
			}
		}

		switch msg.String() {
		case "ctrl+v", "super+v", "cmd+v":
			// Read the OS clipboard (images first, then text). Terminals
			// that intercept Cmd+V still paste via tea.PasteMsg above.
			return m, clipboardPasteCmd()

		case "ctrl+c", "ctrl+d":
			m.quitting = true
			return m, tea.Quit

		case "esc":
			// Esc pauses an active stream immediately. The server
			// cancels the in-flight model request; the stream then
			// closes and streamDoneMsg finishes the turn.
			if m.streaming {
				m.paused = true
				return m, pauseCmd(m.session.ID, m.turnID)
			}

		case "super+a":
			// Select the prompt text only; leave the conversation
			// viewport and its scroll position alone.
			m.input.SelectAll()
			return m, nil

		case "enter":
			text := strings.TrimSpace(m.input.Value())
			if text == "" {
				return m, nil
			}
			m.setMenuVisible(false)
			m.layoutViewport()
			return m.handleInput(text)

		case "tab":
			m.thinkingIdx = (m.thinkingIdx + 1) % len(thinkingLevels)
			return m, nil

		case "up", "down", "pgup", "pgdown", "home", "end":
			// Scroll the conversation viewport. Arrow/page keys scroll
			// half a page (v2's viewport keymap doesn't bind home/end,
			// so handle those explicitly). Scrolling up detaches the
			// view from the live tail; scrolling back to the bottom
			// re-attaches it, so streamed content only pins to the
			// newest output while m.following is true (see
			// refreshViewport).
			var cmd tea.Cmd
			m.viewport, cmd = m.viewport.Update(msg)
			switch msg.String() {
			case "home":
				m.viewport.GotoTop()
			case "end":
				m.viewport.GotoBottom()
			}
			m.following = m.viewport.AtBottom()
			return m, cmd
		}

		// After any other key, update the menu visibility based on input.
		var cmd tea.Cmd
		m.input, cmd = m.input.Update(msg)
		val := m.input.Value()
		if strings.HasPrefix(val, "/") {
			matches := matchCommands(val)
			if len(matches) > 0 {
				m.setMenuVisible(true)
				if m.menuSel >= len(matches) {
					m.menuSel = 0
				}
			} else {
				m.setMenuVisible(false)
			}
		} else {
			m.setMenuVisible(false)
		}
		m.layoutViewport()
		return m, cmd

	case tea.MouseWheelMsg:
		var wcmd tea.Cmd
		m.viewport, wcmd = m.viewport.Update(msg)
		m.following = m.viewport.AtBottom()
		return m, wcmd

	case streamStartMsg:
		m.streamSub = msg.sub
		return m, waitForStreamCmd(msg.sub)

	case modelsLoadedMsg:
		m.overlayEntries = msg.entries
		m.overlaySel = 0
		m.overlayQ = nil
		m.workingMsg = ""
		return m, nil

	case sessionsLoadedMsg:
		m.overlaySessions = msg.sessions
		m.overlayQ = nil
		m.overlaySel = 0
		if m.overlay == "session" {
			// Land on the first real session, not the date header.
			m.overlaySel = m.firstSessionRow()
		}
		m.workingMsg = ""
		return m, nil

	case sessionLoadedMsg:
		// Switching to a different session clears the paused marker and
		// starts pinned to the newest output; reloading the same
		// session (the "saved" path) keeps both.
		sameSession := msg.session.ID == m.session.ID
		if !sameSession {
			m.paused = false
			m.following = true
			// Pending prompt attachments belong to the old session.
			m.pending = nil
			m.previewDirty = true
		}
		m.session = msg.session.info()
		m.selModel = msg.session.Model
		// A reload rebuilds blocks from persisted history, which stores
		// the reasoning text but not how long it took. Carry the measured
		// durations over so the collapsed "Thinking (8.3s)" line keeps
		// its timing when the turn completes — the server's "saved"
		// event triggers exactly this reload.
		prev := m.blocks
		m.blocks = sessionToBlocks(msg.session)
		if sameSession {
			restoreReasoningDurations(m.blocks, prev)
		}
		m.refreshViewport()
		// Subscribe to real-time events — but only if we don't already
		// have a live subscription for this session (the "saved" reload
		// path keeps its subscription running).
		if m.eventSub == nil || m.eventSubID != m.session.ID {
			m.eventSub = nil
			return m, tea.Batch(subscribeCmd(m.session.ID), m.previewCmd())
		}
		return m, m.previewCmd()

	case subStartedMsg:
		m.eventSub = msg.sub
		m.eventSubID = msg.sessionID
		return m, waitForEventCmd(msg.sub, msg.sessionID)

	case subEndedMsg:
		// Server disconnected; reconnect after a delay if this is still
		// our session. subscribeCmd restarts the server if needed.
		if msg.sessionID == m.session.ID && m.session.ID != "" {
			m.eventSub = nil
			return m, delayedCmd(msg.retryAfter, subscribeCmd(m.session.ID))
		}
		return m, nil

	case streamMsg:
		// From our own sendCmd stream. Apply the event and chain.
		m.handleStreamMsg(msg)
		m.refreshViewport()
		if msg.err != nil {
			m.streaming = false
			m.streamSub = nil
			return m, nil
		}
		return m, waitForStreamCmd(m.streamSub)

	case eventMsg:
		// From the SSE subscription — another instance's activity.
		// Drop events from a stale subscription (we switched sessions)
		// but keep draining its own channel so it doesn't back up.
		if msg.sessionID != m.session.ID {
			return m, waitForEventCmd(msg.sub, msg.sessionID)
		}
		// Skip while we're streaming our own message to avoid duplicates.
		if m.streaming {
			return m, waitForEventCmd(msg.sub, msg.sessionID)
		}
		if msg.eventType == "subscribed" {
			// Initial handshake; just keep listening.
			return m, waitForEventCmd(msg.sub, msg.sessionID)
		}
		if msg.eventType == "saved" {
			// Session persisted; reload to get the authoritative state
			// (including reasoning stored on the message). Keep the
			// subscription alive alongside the reload.
			return m, tea.Batch(
				waitForEventCmd(msg.sub, msg.sessionID),
				loadSessionCmd(m.session.ID),
			)
		}
		// content, reasoning, reasoning_end, tool, done, error — apply
		// incrementally for real-time display.
		m.handleStreamMsg(msg.streamMsg)
		m.refreshViewport()
		return m, waitForEventCmd(msg.sub, msg.sessionID)

	case streamDoneMsg:
		// The stream closed; if the model was still reasoning when it
		// ended (e.g. paused mid-thought), record the partial phase.
		m.finalizeReasoning()
		m.finalizeCompaction()
		m.streaming = false
		m.streamSub = nil
		return m, nil

	case errorMSG:
		m.errMsg = msg.err.Error()
		m.workingMsg = ""
		m.overlay = ""
		return m, nil

	case compactDoneMsg:
		m.workingMsg = ""
		if msg.err != nil {
			m.errMsg = msg.err.Error()
			m.refreshViewport()
		}
		return m, nil
	}

	// Forward non-key messages to the input (e.g. tea.Blur).
	var cmd tea.Cmd
	m.input, cmd = m.input.Update(msg)
	return m, cmd
}

// handleInput processes the user's text. For slash commands it runs the
// command locally; for regular text it sends to the server. Returns the
// model and any command to execute.
func (m *tuiModel) handleInput(text string) (tea.Model, tea.Cmd) {
	m.input.SetValue("")
	// The input shrank back to one line; shrink the conversation to match.
	m.layoutViewport()
	m.errMsg = ""

	if !strings.HasPrefix(text, "/") {
		// Regular message: send to server. Any pasted images ride along;
		// the previews and their markers are cleared after the send.
		imgs := make([]imageData, 0, len(m.pending))
		for _, p := range m.pending {
			imgs = append(imgs, p.img)
		}
		m.pending = nil
		m.previewDirty = true
		m.blocks = append(m.blocks, block{kind: "user", text: text})
		m.streaming = true
		m.paused = false
		m.turnID = newTurnID()
		m.following = true // snap to the bottom so the response is visible
		m.refreshViewport()
		// Kick off the request, start the thinking spinner, and delete
		// kitty virtual placements for the now-empty pending set.
		return m, tea.Batch(
			sendCmd(
				m.session.ID, m.turnID, text, imgs,
				m.selProvider.key, m.selProvider.baseURL,
				thinkingLevels[m.thinkingIdx],
				m.selProvider.reasoningField,
				false, "",
			),
			m.spinner.Tick,
			m.previewCmd(),
		)
	}

	// "/compact" (or "/compact ..." with extra focus for the summarizer)
	// folds older turns now, ignoring the 150k auto threshold.
	if text == "/compact" || strings.HasPrefix(text, "/compact ") {
		if m.session.ID == "" {
			m.errMsg = "no session to compact"
			m.refreshViewport()
			return m, nil
		}
		extra := strings.TrimSpace(strings.TrimPrefix(text, "/compact"))
		m.paused = false
		m.following = true
		m.refreshViewport()
		if m.streaming {
			// Interrupt the current model request; compaction events
			// arrive on the existing /send stream.
			return m, compactCmd(m.session.ID, extra)
		}
		m.streaming = true
		m.turnID = newTurnID()
		return m, tea.Batch(
			sendCmd(
				m.session.ID, m.turnID, "", nil,
				m.selProvider.key, m.selProvider.baseURL,
				thinkingLevels[m.thinkingIdx],
				m.selProvider.reasoningField,
				true, extra,
			),
			m.spinner.Tick,
		)
	}

	// "/stats" (or "/stats N" for the last N days) opens the token usage
	// report overlay.
	if text == "/stats" || strings.HasPrefix(text, "/stats ") {
		days := 0
		if fields := strings.Fields(text); len(fields) > 1 {
			if n, err := strconv.Atoi(fields[1]); err == nil && n > 0 {
				days = n
			}
		}
		m.overlay = "stats"
		m.overlayQ = nil
		m.overlaySel = 0
		m.overlayStats = nil
		m.statsDays = days
		m.workingMsg = "loading stats..."
		return m, fetchStatsCmd(days)
	}

	switch text {
	case "/quit", "/exit":
		m.quitting = true
		return m, tea.Quit

	case "/model":
		m.overlay = "model"
		m.overlayQ = nil
		m.overlaySel = 0
		m.blinkOn = true // search cursor visible as soon as the overlay opens
		m.workingMsg = "loading models..."
		return m, fetchModelsCmd(m.providers)

	case "/new":
		cwd, _ := os.Getwd()
		body, _ := json.Marshal(map[string]string{"model": m.selModel, "cwd": cwd})
		var s SessionInfo
		if err := apiPost("/api/sessions", body, &s); err != nil {
			m.errMsg = err.Error()
		} else {
			m.session = s
			m.blocks = nil
			m.paused = false
			m.following = true // a fresh conversation starts at the bottom
			// Retire the old subscription (if any) and subscribe to the
			// new session so other instances' updates stream in.
			m.eventSub = nil
			m.eventSubID = ""
			m.refreshViewport()
			return m, subscribeCmd(s.ID)
		}
		m.refreshViewport()
		return m, nil

	case "/sessions", "/resume":
		m.overlay = "session"
		m.overlayQ = nil
		m.overlaySel = 0
		m.blinkOn = true // search cursor visible as soon as the overlay opens
		m.workingMsg = "loading sessions..."
		return m, listSessionsCmd()

	case "/thinking":
		// Toggle the reasoning display. The blocks stay in the history;
		// only the rendered view changes.
		m.showReasoning = !m.showReasoning
		m.refreshViewport()
		return m, nil

	default:
		m.errMsg = "unknown command: " + text
		m.refreshViewport()
		return m, nil
	}
}

// handleStreamMsg processes one NDJSON event from the server.
func (m *tuiModel) handleStreamMsg(msg streamMsg) {
	if msg.err != nil {
		m.blocks = append(m.blocks, block{kind: "error", text: msg.err.Error()})
		return
	}
	switch msg.eventType {
	case "content":
		// The reasoning phase is over once the answer starts, even if
		// no reasoning_end arrived (defensive against provider quirks).
		m.finalizeReasoning()
		m.finalizeCompaction()
		if len(m.blocks) > 0 && m.blocks[len(m.blocks)-1].kind == "assistant" {
			m.blocks[len(m.blocks)-1].text += msg.text
			m.blocks[len(m.blocks)-1].lines = nil
		} else {
			m.blocks = append(m.blocks, block{kind: "assistant", text: msg.text})
		}
	case "reasoning":
		// Reasoning streams into its own block, marked active until it
		// ends. A new phase after a tool call starts a fresh block so
		// each phase gets its own collapsed timing.
		if len(m.blocks) > 0 && m.blocks[len(m.blocks)-1].kind == "reasoning" && m.blocks[len(m.blocks)-1].active {
			m.blocks[len(m.blocks)-1].text += msg.text
			m.blocks[len(m.blocks)-1].lines = nil
		} else {
			m.blocks = append(m.blocks, block{kind: "reasoning", text: msg.text, active: true, startedAt: time.Now()})
		}
	case "reasoning_end":
		m.finalizeReasoning()
	case "tool":
		// The model called a tool, so the reasoning phase is over.
		m.finalizeReasoning()
		m.blocks = append(m.blocks, block{kind: "tool", text: toolLabel(msg.name, msg.arguments)})
	case "tool_diff":
		// Attach the diff to the most recent tool block that doesn't have
		// one yet — its execution just finished.
		for i := len(m.blocks) - 1; i >= 0; i-- {
			if m.blocks[i].kind == "tool" && m.blocks[i].diff == "" {
				m.blocks[i].diff = msg.diff
				m.blocks[i].lines = nil
				break
			}
		}
	case "compaction":
		m.finalizeReasoning()
		if len(m.blocks) > 0 && m.blocks[len(m.blocks)-1].kind == "compaction" && m.blocks[len(m.blocks)-1].active {
			break
		}
		m.blocks = append(m.blocks, block{kind: "compaction", active: true, startedAt: time.Now()})
	case "compaction_end":
		for i := len(m.blocks) - 1; i >= 0; i-- {
			if m.blocks[i].kind == "compaction" && m.blocks[i].active {
				if msg.text != "" {
					m.blocks[i].text = msg.text
				}
				break
			}
		}
		m.finalizeCompaction()
	case "error":
		m.finalizeCompaction()
		m.blocks = append(m.blocks, block{kind: "error", text: msg.message})
	case "usage":
		// The provider's token count for the latest request. Stored on
		// the session so the status bar indicator survives reloads too.
		if msg.usage != nil {
			m.session.Usage = msg.usage
		}
	case "paused":
		// The stream was stopped (Esc or the last viewer left). The
		// partial output stays on screen; no status indicator is shown.
		m.finalizeReasoning()
		m.finalizeCompaction()
		m.paused = true
	case "done":
		// Stream complete — handled by the caller.
	}
}

// finalizeReasoning marks the in-flight reasoning block (if any) as
// complete and records how long the reasoning phase took. The duration
// backs the collapsed "Thinking (8.3s)" line when reasoning display is
// off. It is idempotent: a block is only finalized once.
func (m *tuiModel) finalizeReasoning() {
	for i := len(m.blocks) - 1; i >= 0; i-- {
		if m.blocks[i].kind == "reasoning" && m.blocks[i].active {
			m.blocks[i].active = false
			m.blocks[i].dur = time.Since(m.blocks[i].startedAt)
			m.blocks[i].lines = nil
			return
		}
	}
}

func (m *tuiModel) finalizeCompaction() {
	for i := len(m.blocks) - 1; i >= 0; i-- {
		if m.blocks[i].kind == "compaction" && m.blocks[i].active {
			m.blocks[i].active = false
			m.blocks[i].dur = time.Since(m.blocks[i].startedAt)
			m.blocks[i].lines = nil
			return
		}
	}
}

// toolLabel renders a tool block header. For file-editing tools the raw
// JSON arguments are replaced with the target path.
func toolLabel(name, arguments string) string {
	var args struct {
		Path string `json:"path"`
	}
	if json.Unmarshal([]byte(arguments), &args) == nil && args.Path != "" {
		return name + ": " + args.Path
	}
	return name + ": " + arguments
}

// renderDiff colorizes a unified diff for display inside the viewport:
// added lines green, deleted lines red, hunk headers dim. Lines are
// wrapped to the width first, then each wrapped segment is colored by
// the kind of the original line, so colors survive wrapping.
func renderDiff(diff string, width int) string {
	var sb strings.Builder
	for _, line := range strings.Split(diff, "\n") {
		if line == "" {
			sb.WriteString("\n")
			continue
		}
		var style lipgloss.Style
		switch line[0] {
		case '+':
			style = styleDiffAdd
		case '-':
			style = styleDiffDel
		case '@':
			style = styleDiffHdr
		}
		for _, seg := range strings.Split(ansi.Wrap(line, width, ""), "\n") {
			sb.WriteString(style.Render(seg) + "\n")
		}
	}
	return sb.String()
}

// updateOverlay handles key events while an overlay (model/session selector)
// is active.
func (m *tuiModel) updateOverlay(msg tea.Msg) (tea.Model, tea.Cmd) {
	switch msg := msg.(type) {
	case tea.WindowSizeMsg:
		prevW := m.width
		m.width = msg.Width
		m.height = msg.Height
		// Relayout the viewport even while an overlay is open, otherwise
		// it keeps its initial 80x20 size and the chat prompt renders
		// mid-screen after the overlay closes.
		m.layoutViewport()
		if msg.Width != prevW {
			m.refreshViewport()
		}
		return m, nil

	case blinkMsg:
		m.blinkOn = !m.blinkOn
		return m, blinkCmd()

	case modelsLoadedMsg:
		m.overlayEntries = msg.entries
		m.overlaySel = 0
		m.overlayQ = nil
		m.workingMsg = ""
		return m, nil

	case sessionsLoadedMsg:
		m.overlaySessions = msg.sessions
		m.overlayQ = nil
		m.overlaySel = 0
		if m.overlay == "session" {
			// Land on the first real session, not the date header.
			m.overlaySel = m.firstSessionRow()
		}
		m.workingMsg = ""
		return m, nil

	case statsLoadedMsg:
		m.overlayStats = &msg.report
		m.overlaySel = 0
		m.workingMsg = ""
		return m, nil

	case errorMSG:
		m.errMsg = msg.err.Error()
		m.overlay = ""
		m.workingMsg = ""
		return m, nil

	case subStartedMsg:
		m.eventSub = msg.sub
		m.eventSubID = msg.sessionID
		return m, waitForEventCmd(msg.sub, msg.sessionID)

	case eventMsg:
		// SSE events keep flowing while an overlay is open. Queue a
		// reload on "saved" and keep the chain alive.
		if msg.sessionID != m.session.ID {
			return m, waitForEventCmd(msg.sub, msg.sessionID)
		}
		if msg.eventType == "saved" {
			if !m.streaming {
				return m, tea.Batch(
					waitForEventCmd(msg.sub, msg.sessionID),
					loadSessionCmd(m.session.ID),
				)
			}
		}
		return m, waitForEventCmd(msg.sub, msg.sessionID)

	case subEndedMsg:
		if msg.sessionID == m.session.ID && m.session.ID != "" {
			m.eventSub = nil
			return m, delayedCmd(msg.retryAfter, subscribeCmd(m.session.ID))
		}
		return m, nil
	}

	keyMsg, ok := msg.(tea.KeyPressMsg)
	if !ok {
		return m, nil
	}

	switch keyMsg.String() {
	case "esc":
		m.overlay = ""
		m.overlayQ = nil
		m.workingMsg = ""
		return m, nil

	case "enter":
		return m.confirmOverlay()

	case "up":
		if m.overlay == "stats" {
			if m.overlaySel > 0 {
				m.overlaySel--
			}
		} else if m.overlay == "session" {
			m.moveSessionSel(-1)
		} else if m.overlaySel > 0 {
			m.overlaySel--
		}
		return m, nil

	case "down":
		if m.overlay == "stats" {
			if m.overlaySel < m.statsScrollMax() {
				m.overlaySel++
			}
		} else if m.overlay == "session" {
			m.moveSessionSel(1)
		} else {
			cnt := m.overlayCount()
			if m.overlaySel < cnt-1 {
				m.overlaySel++
			}
		}
		return m, nil

	case "backspace":
		if m.overlay == "stats" {
			return m, nil
		}
		if len(m.overlayQ) > 0 {
			m.overlayQ = m.overlayQ[:len(m.overlayQ)-1]
			if m.overlay == "session" {
				m.overlaySel = m.firstSessionRow()
			} else {
				m.overlaySel = 0
			}
		}
		return m, nil
	}

	// Printable characters (including space) append to the search query.
	// Space arrives as "space" via String() but its Text is " ", so
	// checking Text catches both letters and spaces, while control keys
	// (ctrl+c, arrows, etc.) have empty Text and are ignored. The stats
	// overlay has no search field, so keys fall through to nothing.
	if (m.overlay == "model" || m.overlay == "session") && len(keyMsg.Text) > 0 {
		m.overlayQ = append(m.overlayQ, keyMsg.Text...)
		if m.overlay == "session" {
			m.overlaySel = m.firstSessionRow()
		} else {
			m.overlaySel = 0
		}
	}
	return m, nil
}

// confirmOverlay handles Enter in an overlay: selects the highlighted item
// and closes the overlay.
func (m *tuiModel) confirmOverlay() (tea.Model, tea.Cmd) {
	if m.overlay == "model" {
		filtered := filterEntries(m.overlayEntries, m.overlayQ)
		if len(filtered) == 0 {
			return m, nil
		}
		if m.overlaySel >= len(filtered) {
			m.overlaySel = 0
		}
		e := filtered[m.overlaySel]
		m.selProvider = e.provider
		m.selModel = e.model
		// Remember the choice as the default for future launches.
		_ = saveLastModel(e.provider.name, e.model)

		// Mid-session switch: update the current session's model in place.
		// The conversation history stays; only the model that answers
		// future turns changes.
		if m.session.ID != "" {
			body, _ := json.Marshal(map[string]string{"model": e.model})
			if err := apiPatch("/api/sessions/"+m.session.ID, body, nil); err != nil {
				m.errMsg = err.Error()
			} else {
				m.session.Model = e.model
			}
			m.overlay = ""
			m.overlayQ = nil
			m.workingMsg = ""
			m.refreshViewport()
			return m, nil
		}

		// No session yet (the startup selector): create one with the
		// selected model.
		cwd, _ := os.Getwd()
		body, _ := json.Marshal(map[string]string{"model": e.model, "cwd": cwd})
		var s SessionInfo
		if err := apiPost("/api/sessions", body, &s); err != nil {
			m.errMsg = err.Error()
		} else {
			m.session = s
			m.blocks = nil
			m.paused = false
			m.pending = nil
			m.previewDirty = true
			// Subscribe to the new session so updates from other
			// instances viewing it stream in live.
			m.eventSub = nil
			m.eventSubID = ""
			m.overlay = ""
			m.overlayQ = nil
			m.workingMsg = ""
			m.refreshViewport()
			return m, tea.Batch(subscribeCmd(s.ID), m.previewCmd())
		}
		m.overlay = ""
		m.overlayQ = nil
		m.workingMsg = ""
		m.refreshViewport()
		return m, nil
	}

	if m.overlay == "session" {
		rows := m.sessionRows()
		if m.overlaySel < 0 || m.overlaySel >= len(rows) || rows[m.overlaySel].date {
			m.overlaySel = m.firstSessionRow()
		}
		if m.overlaySel >= len(rows) {
			return m, nil
		}
		picked := rows[m.overlaySel].sess
		m.overlay = ""
		m.overlayQ = nil
		m.workingMsg = ""
		// Fetch the full session (with messages) in the background.
		return m, loadSessionCmd(picked.ID)
	}

	if m.overlay == "stats" {
		// Nothing to select in the report; Enter just closes it.
		m.overlay = ""
		m.overlayQ = nil
		m.workingMsg = ""
		return m, nil
	}

	return m, nil
}

// overlayCount returns the number of filtered items in the active overlay.
func (m *tuiModel) overlayCount() int {
	if m.overlay == "model" {
		return len(filterEntries(m.overlayEntries, m.overlayQ))
	}
	if m.overlay == "session" {
		return len(filterSessions(m.overlaySessions, m.overlayQ))
	}
	return 0
}

// sessionRow is one line in the session picker: either a non-selectable
// date group header or a selectable session entry.
type sessionRow struct {
	date  bool
	label string
	sess  *SessionInfo
}

// sessionRows flattens the filtered sessions into picker rows, inserting
// "Today"/"Yesterday"/date headers between groups of sessions. Sessions
// arrive sorted by UpdatedAt, so the groups come out in order.
func (m tuiModel) sessionRows() []sessionRow {
	var rows []sessionRow
	last := ""
	for _, s := range filterSessions(m.overlaySessions, m.overlayQ) {
		h := dayLabel(s.UpdatedAt)
		if h != last {
			last = h
			rows = append(rows, sessionRow{date: true, label: h})
		}
		// The server only lists sessions with messages, so every row
		// has a title.
		rows = append(rows, sessionRow{label: s.Title, sess: &s})
	}
	return rows
}

// firstSessionRow returns the index of the first selectable (non-header)
// row, or len(rows) when there are no selectable rows.
func (m tuiModel) firstSessionRow() int {
	rows := m.sessionRows()
	for i, r := range rows {
		if !r.date {
			return i
		}
	}
	return len(rows)
}

// moveSessionSel moves the session picker selection by one session in the
// given direction, skipping date header rows.
func (m *tuiModel) moveSessionSel(dir int) {
	rows := m.sessionRows()
	for i := m.overlaySel + dir; i >= 0 && i < len(rows); i += dir {
		if !rows[i].date {
			m.overlaySel = i
			return
		}
	}
}

// dayLabel returns the group header for a time: "Today" or "Yesterday"
// when recent, otherwise the weekday and date (e.g. "Friday, Aug 21").
func dayLabel(t time.Time) string {
	day := t.Format("2006-01-02")
	switch day {
	case time.Now().Format("2006-01-02"):
		return "Today"
	case time.Now().AddDate(0, 0, -1).Format("2006-01-02"):
		return "Yesterday"
	}
	return t.Format("Monday, Jan 2")
}

// innerWidth is the content width after the left/right inset.
func (m tuiModel) innerWidth() int {
	w := m.width - 2*tuiHPad
	if w < 1 {
		w = 1
	}
	return w
}

// inputWidth returns the prompt input width: the inset content width
// minus the "> " prompt prefix.
func (m tuiModel) inputWidth() int {
	w := m.innerWidth() - 2
	if w < 1 {
		w = 1
	}
	return w
}

// inputHeight returns how many rows the prompt input needs to display its
// value wrapped to the input width, at least 1. The input grows as the
// prompt wraps to more lines — up to the whole terminal minus the status
// bar, the prompt borders, and one viewport row — so a long prompt is
// never truncated: the conversation viewport shrinks to make room instead.
func (m tuiModel) inputHeight() int {
	lines := len(strings.Split(ansi.Wrap(m.input.Value(), m.inputWidth(), ""), "\n"))
	if lines < 1 {
		lines = 1
	}
	max := m.height - 4 // status bar + prompt borders + one viewport row
	if max < 1 {
		max = 1
	}
	if lines > max {
		lines = max
	}
	return lines
}

// layoutViewport sizes the viewport to fill all rows except the status bar
// (1 row), the prompt top and bottom borders (2 rows), the input
// (inputHeight rows), and any menu lines. The input grows as the prompt
// wraps to more lines, so a long prompt pushes the conversation up
// instead of truncating it.
func (m *tuiModel) layoutViewport() {
	m.input.SetWidth(m.inputWidth())
	inH := m.inputHeight()
	m.input.SetHeight(inH)
	// When the input's height changes, re-set the value so the
	// textarea's internal scroll snaps back to the top. It scrolls down
	// to follow the cursor while the input is still short, which would
	// otherwise leave the view stuck showing only the last lines.
	if inH != m.lastInH {
		m.input.SetValue(m.input.Value())
		m.lastInH = inH
	}

	reserved := 3 + inH // status bar + prompt top/bottom borders + input
	if m.menuVisible {
		// The slash menu overlays the preview (and eats extra rows if
		// it's taller), so graphics are hidden while it's open.
		n := len(matchCommands(m.input.Value()))
		if n > 0 {
			reserved += n
		}
	} else {
		reserved += m.previewRowCount()
	}
	vpHeight := m.height - reserved
	if vpHeight < 1 {
		vpHeight = 1
	}
	m.viewport.SetWidth(m.innerWidth())
	m.viewport.SetHeight(vpHeight)
}

func (m *tuiModel) setMenuVisible(v bool) {
	m.menuVisible = v
}

// previewRowCount returns the rows reserved for kitty-graphics
// thumbnails above the input. Terminals without image protocol get
// zero: only the inline IMG chip in the prompt is shown.
func (m tuiModel) previewRowCount() int {
	if !kittyTerminal() {
		return 0
	}
	max := 0
	for _, p := range m.pending {
		if p.rows > max {
			max = p.rows
		}
	}
	return max
}

func (b block) linesValid(width int, showReasoning bool) bool {
	if b.lines == nil || b.lineWidth != width {
		return false
	}
	if b.kind == "reasoning" && b.lineShowR != showReasoning {
		return false
	}
	return true
}

// renderBlock renders one conversation block, wrapped to width. While
// reasoning display is off (see /thinking), a reasoning block collapses
// to a summary line instead of its full text.
func (m tuiModel) renderBlock(b *block, width int) string {
	switch b.kind {
	case "user":
		return styleUser.Render("you: ") + ansi.Wrap(b.text, width, "") + "\n\n"
	case "assistant":
		return styleAssistant.Render(ansi.Wrap(b.text, width, "")) + "\n\n"
	case "reasoning":
		if m.showReasoning {
			return styleReasoning.Render(ansi.Wrap(b.text, width, "")) + "\n"
		}
		return styleReasoning.Render(m.reasoningLabel(*b)) + "\n"
	case "compaction":
		if b.active {
			return styleReasoning.Render("Compacting...") + "\n"
		}
		s := styleReasoning.Render(m.compactionLabel(*b)) + "\n"
		if b.text != "" {
			s += styleAssistant.Render(ansi.Wrap(b.text, width, "")) + "\n\n"
		}
		return s
	case "tool":
		s := styleTool.Width(width).Render(ansi.Wrap("⟢ "+b.text, width, "")) + "\n"
		if b.diff != "" {
			s += renderDiff(b.diff, width)
		}
		return s + "\n"
	case "error":
		return styleError.Render("error: "+ansi.Wrap(b.text, width, "")) + "\n\n"
	}
	return ""
}

// renderBlocks renders the conversation blocks into the viewport text,
// wrapped to width. While reasoning display is off (see /thinking), each
// reasoning block collapses to a summary line instead of its full text.
func (m tuiModel) renderBlocks(width int) string {
	var sb strings.Builder
	for i := range m.blocks {
		sb.WriteString(m.renderBlock(&m.blocks[i], width))
	}
	return sb.String()
}

// reasoningLabel renders the collapsed reasoning line shown while
// reasoning display is off: "Thinking..." while the block is still
// streaming, "Thinking (8.3s)" once it finished. The duration is
// omitted when unknown (e.g. reasoning loaded from session history).
func (m tuiModel) reasoningLabel(b block) string {
	if b.active {
		return "Thinking..."
	}
	if b.dur > 0 {
		return fmt.Sprintf("Thinking (%.1fs)", b.dur.Seconds())
	}
	return "Thinking"
}

func (m tuiModel) compactionLabel(b block) string {
	if b.active {
		return "Compacting..."
	}
	if b.dur > 0 {
		return fmt.Sprintf("Compacted (%.1fs)", b.dur.Seconds())
	}
	return "Compacted"
}

// refreshViewport updates the viewport from cached per-block lines.
// Finalized blocks are reused; only dirty blocks (typically the active
// streaming block) are re-wrapped. The live line slice is spliced from
// the first dirty block onward and passed to SetContentLines so the
// viewport does not re-split a full-history string. It follows the
// newest output only while m.following is true; after the user scrolls
// up, the current offset is kept so streamed content never yanks the
// view away from scrollback.
func (m *tuiModel) refreshViewport() {
	width := m.innerWidth()
	if width < 10 {
		width = 10
	}
	if width != m.contentWidth {
		for i := range m.blocks {
			m.blocks[i].lines = nil
		}
		m.contentWidth = width
		m.rebuildContentFrom(0, width)
	} else {
		first := -1
		for i := range m.blocks {
			if !m.blocks[i].linesValid(width, m.showReasoning) {
				first = i
				break
			}
		}
		if first >= 0 {
			m.rebuildContentFrom(first, width)
		} else if len(m.blockStart) != len(m.blocks) {
			m.rebuildContentFrom(0, width)
		}
	}
	lines := m.contentLines
	if len(lines) == 0 {
		lines = []string{""}
	}
	m.viewport.SetContentLines(lines)
	if m.following {
		m.viewport.GotoBottom()
	}
}

func (m *tuiModel) rebuildContentFrom(idx, width int) {
	start := 0
	if idx > 0 && idx < len(m.blockStart) && m.blockStart[idx] <= len(m.contentLines) {
		start = m.blockStart[idx]
	} else {
		idx = 0
	}
	m.contentLines = m.contentLines[:start]
	m.blockStart = m.blockStart[:idx]
	last := len(m.blocks) - 1
	for i := idx; i < len(m.blocks); i++ {
		m.blockStart = append(m.blockStart, len(m.contentLines))
		lines := m.ensureBlockLines(i, width)
		// strings.Split keeps a trailing "" for the final \n. Joining
		// those across blocks would insert an extra blank line; drop it
		// on every block except the last so the slice matches
		// Split(renderBlocks(width), "\n").
		if i < last && len(lines) > 0 && lines[len(lines)-1] == "" {
			lines = lines[:len(lines)-1]
		}
		m.contentLines = append(m.contentLines, lines...)
	}
}

func (m *tuiModel) ensureBlockLines(i, width int) []string {
	b := &m.blocks[i]
	if b.linesValid(width, m.showReasoning) {
		return b.lines
	}
	b.lines = strings.Split(m.renderBlock(b, width), "\n")
	b.lineWidth = width
	b.lineShowR = m.showReasoning
	return b.lines
}

// previewPaintedMsg signals that the preview repaint command finished.
type previewPaintedMsg struct{}

// addImage appends a pasted image to the prompt. The data must already
// be a recognizable image; the caller places the [n] marker at the
// cursor. The preview box is derived from the image's aspect ratio.
func (m *tuiModel) addImage(name string, data []byte) error {
	if len(data) > maxImageSourceBytes {
		return fmt.Errorf("image too large: %d bytes (limit %d)", len(data), maxImageSourceBytes)
	}
	data, mime, err := normalizeImage(data)
	if err != nil {
		return err
	}
	p := pendingImage{
		img:  imageData{MIME: mime, Data: base64.StdEncoding.EncodeToString(data)},
		name: name,
	}
	if w, h, err := imageSize(data); err == nil {
		p.cols, p.rows = previewBox(w, h)
	} else if kittyTerminal() {
		p.cols, p.rows = 6, 3
	}
	m.pending = append(m.pending, p)
	m.previewDirty = true
	// A best-effort hint when the current model looks text-only.
	if !modelSupportsImages(m.selModel) && m.errMsg == "" {
		m.errMsg = fmt.Sprintf("note: %s may not support images", m.selModel)
	}
	return nil
}

// pasteImage attaches a pasted image and inserts its [n] marker at the
// input cursor (the insertion point).
func (m *tuiModel) pasteImage(name string, data []byte) error {
	if err := m.addImage(name, data); err != nil {
		return err
	}
	m.input.InsertString(imageMarker(len(m.pending)) + " ")
	m.layoutViewport()
	return nil
}

// clipboardPasteMsg is the result of an async OS clipboard read.
type clipboardPasteMsg struct {
	content clipboardContent
}

func clipboardPasteCmd() tea.Cmd {
	return func() tea.Msg {
		return clipboardPasteMsg{content: readClipboard()}
	}
}

func (m *tuiModel) applyClipboardPaste(msg clipboardPasteMsg) (tea.Model, tea.Cmd) {
	c := msg.content
	if len(c.data) > 0 {
		if err := m.pasteImage(c.name, c.data); err != nil {
			m.errMsg = err.Error()
		}
		return m, m.previewCmd()
	}
	if c.text != "" {
		if files := localImagesFromPaste(c.text); len(files) > 0 {
			return m.pasteLocalImages(files)
		}
		m.input.InsertString(c.text)
		m.layoutViewport()
	}
	return m, nil
}

func (m *tuiModel) pasteLocalImages(files []localImageFile) (tea.Model, tea.Cmd) {
	for _, f := range files {
		if err := m.pasteImage(f.name, f.data); err != nil {
			m.errMsg = err.Error()
		}
	}
	return m, m.previewCmd()
}

// pasteMixedContent handles a paste that interleaves text and OSC 1337
// images: the images become previews and the text inserts in order, so a
// paste of "see <image> here" lands as "see [1] here".
func (m *tuiModel) pasteMixedContent(content string) (tea.Model, tea.Cmd) {
	var sb strings.Builder
	for _, seg := range splitPasteSegments(content) {
		if seg.data != nil {
			if err := m.addImage("", seg.data); err != nil {
				m.errMsg = err.Error()
				continue // the failed image contributes nothing
			}
			sb.WriteString(imageMarker(len(m.pending)) + " ")
			continue
		}
		sb.WriteString(seg.text)
	}
	m.input.InsertString(sb.String())
	m.layoutViewport()
	return m, m.previewCmd()
}

// handleKittyGraphics processes a Kitty graphics event from the input
// stream: terminals paste clipboard images as _G transmissions. Chunked
// transmissions accumulate by id until the final chunk (m=0) arrives.
func (m *tuiModel) handleKittyGraphics(ev uv.KittyGraphicsEvent) (tea.Model, tea.Cmd) {
	opts := ev.Options
	// Delete and query actions carry no pasted image data.
	if opts.Action == 'd' || opts.Action == 'q' {
		return m, nil
	}
	if opts.Chunk {
		// Intermediate chunk: accumulate by transmission id.
		if m.kittyChunks == nil {
			m.kittyChunks = map[int][]byte{}
		}
		m.kittyChunks[opts.ID] = append(m.kittyChunks[opts.ID], ev.Payload...)
		return m, nil
	}
	payload := ev.Payload
	if prev, ok := m.kittyChunks[opts.ID]; ok {
		payload = append(prev, payload...)
		delete(m.kittyChunks, opts.ID)
	}
	if len(payload) == 0 {
		return m, nil
	}
	data, ok := kittyPasteData(payload, opts.Format, opts.ImageWidth, opts.ImageHeight)
	if !ok {
		return m, nil
	}
	if err := m.pasteImage("", data); err != nil {
		m.errMsg = err.Error()
	}
	return m, m.previewCmd()
}

// previewCmd returns a command that transmits or deletes kitty virtual
// placements when the pending image set has changed. tea.Batch drops
// nil commands, so it can be batched unconditionally.
func (m *tuiModel) previewCmd() tea.Cmd {
	if !m.previewDirty {
		return nil
	}
	m.previewDirty = false
	if !kittyTerminal() || !isTerminal(os.Stdout) {
		return nil
	}
	// Snapshot pending at command creation so the paint goroutine never
	// reads the model concurrently. Overlay and menu omit placeholders
	// in View; virtual placements stay until pending itself changes.
	entries := []previewPlacement{}
	for i, p := range m.pending {
		if p.cols <= 0 {
			continue
		}
		data, err := base64.StdEncoding.DecodeString(p.img.Data)
		if err == nil {
			entries = append(entries, previewPlacement{
				data: data,
				num:  i + 1,
				cols: p.cols,
				rows: p.rows,
			})
		}
	}
	return func() tea.Msg {
		paintKittyPreviews(entries)
		return previewPaintedMsg{}
	}
}

// renderPreviews renders Unicode placeholder cells for kitty virtual
// placements, top-aligned in the reserved band above the prompt.
// Terminals without the protocol show nothing here; the IMG chip in
// the prompt is the only indicator.
func (m tuiModel) renderPreviews() string {
	n := m.previewRowCount()
	if n == 0 {
		return ""
	}
	type item struct {
		cols, rows int
		lines      []string
	}
	var items []item
	for i, p := range m.pending {
		if p.cols <= 0 {
			continue
		}
		items = append(items, item{
			cols:  p.cols,
			rows:  p.rows,
			lines: strings.Split(placeholderGrid(i+1, p.cols, p.rows), "\n"),
		})
	}
	lines := make([]string, n)
	for y := 0; y < n; y++ {
		var sb strings.Builder
		for j, it := range items {
			if j > 0 {
				sb.WriteString(strings.Repeat(" ", previewGap))
			}
			if y < it.rows && y < len(it.lines) {
				sb.WriteString(it.lines[y])
			} else {
				sb.WriteString(strings.Repeat(" ", it.cols))
			}
		}
		if sb.Len() == 0 {
			lines[y] = " "
		} else {
			lines[y] = sb.String()
		}
	}
	return strings.Join(lines, "\n")
}

func imageMarker(n int) string {
	return fmt.Sprintf("[IMG %d]", n)
}

func imageChip(n int) string {
	return styleImgChip.Render(fmt.Sprintf("IMG %d", n))
}

// --- view ---

func (m tuiModel) View() tea.View {
	if m.quitting {
		return tea.NewView("")
	}

	// If an overlay is active, render it instead of the chat view.
	if m.overlay != "" {
		v := tea.NewView(m.viewOverlay())
		v.AltScreen = true
		return v
	}

	// Status bar: model + thinking level + token usage + status. The
	// usage indicator mirrors opencode's context meter: total tokens of
	// the latest request and its share of the model's context window.
	status := fmt.Sprintf("%s (%s)", m.selModel, thinkingLevels[m.thinkingIdx])
	if !m.showReasoning {
		status += "  reasoning:hidden"
	}
	if usage := m.usageString(); usage != "" {
		status += "  " + usage
	}
	// While streaming, show the animated spinner instead of the old
	// "thinking..." text; otherwise show any working/error message.
	if m.streaming {
		status += "  " + m.spinner.View()
	} else if suffix := m.statusSuffix(); suffix != "" {
		status += "  " + suffix
	}
	statusLine := styleDim.Render(status)

	// Input area: "> " prompt plus the (possibly multi-line) textarea.
	// The textarea draws and blinks its own cursor, so just render its
	// view. It ends with a newline; strip it so the join below doesn't
	// leave a blank row. Image markers render as chips of the same
	// width as "[IMG n]" so the textarea cursor stays aligned.
	inputView := strings.TrimSuffix(m.input.View(), "\n")
	for i := len(m.pending); i >= 1; i-- {
		inputView = strings.ReplaceAll(inputView, imageMarker(i), imageChip(i))
	}
	inputLine := stylePrompt.Render("> ") + inputView
	border := stylePromptBorder.Render(strings.Repeat("─", m.innerWidth()))

	// Build the slash-command menu if visible. It renders above the
	// input line so it doesn't push content down.
	menuStr := ""
	if m.menuVisible {
		menuStr = m.renderMenu()
	}

	// Assemble: viewport, then either the slash menu or the image
	// preview placeholders, then the prompt. Preview rows are Unicode
	// placeholders in the footer chrome (same fixed band as the status
	// line). The menu replaces the preview slot, so placeholders are
	// omitted while it is open; virtual placements stay until pending
	// changes.
	// Viewport first, padded to its allocated height so the preview
	// band sits in the reserved rows above the prompt.
	vp := strings.TrimRight(m.viewport.View(), "\n")
	vpLines := strings.Split(vp, "\n")
	for len(vpLines) < m.viewport.Height() {
		vpLines = append(vpLines, "")
	}
	if h := m.viewport.Height(); h > 0 && len(vpLines) > h {
		vpLines = vpLines[:h]
	}
	parts := []string{strings.Join(vpLines, "\n")}
	if menuStr != "" {
		parts = append(parts, menuStr)
	} else if previewStr := m.renderPreviews(); previewStr != "" {
		parts = append(parts, previewStr)
	}
	parts = append(parts, border, inputLine, border, statusLine)
	frame := lipgloss.NewStyle().Padding(0, tuiHPad).Render(strings.Join(parts, "\n"))
	v := tea.NewView(frame)
	v.AltScreen = true
	return v
}

// renderMenu renders the slash-command autocomplete menu. Each matching
// command is shown on its own line with the selected item highlighted.
func (m tuiModel) renderMenu() string {
	matches := matchCommands(m.input.Value())
	if len(matches) == 0 {
		return ""
	}
	var sb strings.Builder
	for i, c := range matches {
		if i == m.menuSel {
			sb.WriteString(styleSelected.Render(c.name) + "  " + styleDim.Render(c.desc))
		} else {
			sb.WriteString(styleDim.Render(c.name) + "  " + styleDim.Render(c.desc))
		}
		if i < len(matches)-1 {
			sb.WriteString("\n")
		}
	}
	return sb.String()
}

// statusSuffix returns a short status string for the status bar. The
// streaming and paused states are no longer shown as text: streaming uses
// the animated spinner (see View), and a paused stream shows no indicator.
func (m tuiModel) statusSuffix() string {
	if m.workingMsg != "" {
		return m.workingMsg
	}
	if m.errMsg != "" {
		return m.errMsg
	}
	return ""
}

// usageString renders the session's token usage for the status bar,
// mirroring opencode's context meter: "8.4K (7%)" — the total tokens of
// the latest model request and its share of the model's context window.
// Empty until the provider reports usage.
func (m tuiModel) usageString() string {
	u := m.session.Usage
	if u == nil || u.TotalTokens <= 0 {
		return ""
	}
	s := formatTokens(u.TotalTokens)
	if w := contextWindowTokens(m.selModel); w > 0 {
		return fmt.Sprintf("%s (%d%%)", s, u.TotalTokens*100/w)
	}
	return s
}

// viewOverlay renders the model selector, session picker, or stats report.
func (m tuiModel) viewOverlay() string {
	if m.workingMsg != "" {
		return styleDim.Render(m.workingMsg)
	}

	var sb strings.Builder
	if m.overlay == "model" {
		sb.WriteString(m.renderModelSelector())
	} else if m.overlay == "session" {
		sb.WriteString(m.renderSessionSelector())
	} else if m.overlay == "stats" {
		sb.WriteString(m.renderStatsOverlay())
	}
	return sb.String()
}

// renderStatsOverlay renders the /stats token usage report with a
// scrollable window. The report can outgrow the terminal, so ↑↓ scroll
// the window; Esc or Enter closes the overlay.
func (m tuiModel) renderStatsOverlay() string {
	var sb strings.Builder
	width := m.width
	if width < 10 {
		width = 10
	}

	window := "all time"
	if m.statsDays > 0 {
		window = fmt.Sprintf("last %d days", m.statsDays)
	}
	sb.WriteString(styleDim.Render(fmt.Sprintf("Token usage (%s) — ↑↓ to scroll, Esc to close", window)) + "\n\n")

	if m.overlayStats == nil {
		sb.WriteString(styleDim.Render(m.workingMsg))
		return sb.String()
	}

	lines := renderStats(*m.overlayStats, width-4, true)
	visible := m.height - 6
	if visible < 3 {
		visible = 3
	}
	scroll := m.overlaySel
	maxScroll := len(lines) - visible
	if scroll > maxScroll {
		scroll = maxScroll
	}
	if scroll < 0 {
		scroll = 0
	}
	end := scroll + visible
	if end > len(lines) {
		end = len(lines)
	}
	for i := scroll; i < end; i++ {
		sb.WriteString(lines[i] + "\n")
	}
	if len(lines) > visible {
		sb.WriteString("\n" + styleDim.Render(fmt.Sprintf("%d–%d of %d lines", scroll+1, end, len(lines))))
	}
	return sb.String()
}

// statsScrollMax returns the highest allowed scroll offset for the
// stats overlay: the report length minus one visible page.
func (m tuiModel) statsScrollMax() int {
	if m.overlayStats == nil {
		return 0
	}
	lines := renderStats(*m.overlayStats, m.width-4, false)
	visible := m.height - 6
	if visible < 3 {
		visible = 3
	}
	max := len(lines) - visible
	if max < 0 {
		max = 0
	}
	return max
}

// renderModelSelector renders the model picker overlay.
func (m tuiModel) renderModelSelector() string {
	var sb strings.Builder
	filtered := filterEntries(m.overlayEntries, m.overlayQ)
	width := m.width
	if width < 10 {
		width = 10
	}

	sb.WriteString(styleDim.Render("Select a model — type to search, ↑↓ to navigate, Enter to select, Esc to cancel") + "\n\n")

	// Search field: prompt, query text, and a blinking cursor.
	cur := " "
	if m.blinkOn {
		cur = styleCursor.Render("█")
	}
	sb.WriteString(styleCursor.Render("> ") + string(m.overlayQ) + cur + "\n\n")

	if len(filtered) == 0 {
		sb.WriteString(styleDim.Render("no matches"))
		return sb.String()
	}

	maxItems := m.height - 6
	if maxItems < 3 {
		maxItems = 3
	}
	scroll := 0
	if m.overlaySel < scroll {
		scroll = m.overlaySel
	} else if m.overlaySel >= scroll+maxItems {
		scroll = m.overlaySel - maxItems + 1
	}
	end := scroll + maxItems
	if end > len(filtered) {
		end = len(filtered)
	}
	for i := scroll; i < end; i++ {
		e := filtered[i]
		label := e.provider.name + "  " + e.model
		if i == m.overlaySel {
			sb.WriteString(styleSelected.Render("▸ "+ansi.Wrap(label, width, "")) + "\n")
		} else {
			sb.WriteString(styleDim.Render(e.provider.name) + "  " + ansi.Wrap(e.model, width, "") + "\n")
		}
	}
	sb.WriteString("\n" + styleDim.Render(fmt.Sprintf("%d/%d models", m.overlaySel+1, len(filtered))))
	return sb.String()
}

// renderSessionSelector renders the session picker overlay: a search line
// that always holds focus, then sessions grouped under Today/Yesterday/
// date headers. ↑↓ move between sessions, skipping the headers.
func (m tuiModel) renderSessionSelector() string {
	var sb strings.Builder
	rows := m.sessionRows()
	width := m.width
	if width < 10 {
		width = 10
	}

	sb.WriteString(styleDim.Render("Switch session — type to search, ↑↓ to navigate, Enter to select, Esc to cancel") + "\n\n")

	// Search field, always focused; "search" shows as a placeholder, and
	// a blinking cursor marks the typing position.
	q := string(m.overlayQ)
	if q == "" {
		q = styleDim.Render("search")
	}
	cur := " "
	if m.blinkOn {
		cur = styleCursor.Render("█")
	}
	sb.WriteString(styleCursor.Render("> ") + q + cur + "\n\n")

	if len(rows) == 0 {
		sb.WriteString(styleDim.Render("no matches"))
		return sb.String()
	}

	maxItems := m.height - 6
	if maxItems < 3 {
		maxItems = 3
	}
	scroll := 0
	if m.overlaySel < scroll {
		scroll = m.overlaySel
	} else if m.overlaySel >= scroll+maxItems {
		scroll = m.overlaySel - maxItems + 1
	}
	end := scroll + maxItems
	if end > len(rows) {
		end = len(rows)
	}
	sel, total := 0, 0
	for i := scroll; i < end; i++ {
		r := rows[i]
		if r.date {
			sb.WriteString(styleDim.Render(r.label) + "\n")
			continue
		}
		marker := "  "
		if r.sess.ID == m.session.ID {
			marker = "→ "
		}
		if i == m.overlaySel {
			sb.WriteString(styleSelected.Render("▸ " + ansi.Wrap(marker+r.label, width, "")) + "\n")
		} else {
			sb.WriteString(ansi.Wrap(marker+r.label, width, "") + "\n")
		}
	}
	// Count selectable rows for the footer (headers don't count).
	for i, r := range rows {
		if r.date {
			continue
		}
		total++
		if i <= m.overlaySel {
			sel++
		}
	}
	sb.WriteString("\n" + styleDim.Render(fmt.Sprintf("%d/%d sessions", sel, total)))
	return sb.String()
}

// runTUI starts the Bubble Tea program.
func runTUI(providers []provider, selProvider provider, selModel string, session SessionInfo) {
	m := initialModel(providers, selProvider, selModel, session)
	p := tea.NewProgram(&m)
	if _, err := p.Run(); err != nil {
		fmt.Fprintf(os.Stderr, "error: %v\n", err)
		os.Exit(1)
	}
}
