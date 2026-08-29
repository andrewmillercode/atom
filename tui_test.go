package main

import (
	"fmt"
	"reflect"
	"strings"
	"testing"
	"time"

	"charm.land/bubbles/v2/textarea"
	tea "charm.land/bubbletea/v2"
	"charm.land/lipgloss/v2"
	"github.com/charmbracelet/x/ansi"
)

func TestFormatTokens(t *testing.T) {
	cases := []struct {
		n    int
		want string
	}{
		{0, "0"},
		{312, "312"},
		{999, "999"},
		{1000, "1.0K"},
		{8400, "8.4K"},
		{128000, "128.0K"},
		{1234567, "1.2M"},
	}
	for _, c := range cases {
		if got := formatTokens(c.n); got != c.want {
			t.Errorf("formatTokens(%d) = %q, want %q", c.n, got, c.want)
		}
	}
}

func TestContextWindowTokens(t *testing.T) {
	cases := []struct {
		model string
		want  int
	}{
		{"deepseek-v4-flash:cloud", 128000},
		{"qwen3.5:cloud", 128000},
		{"llama3.3:70b", 128000},
		{"some-unknown-model", 128000},
	}
	for _, c := range cases {
		if got := contextWindowTokens(c.model); got != c.want {
			t.Errorf("contextWindowTokens(%q) = %d, want %d", c.model, got, c.want)
		}
	}
}

func TestParseUsageEvent(t *testing.T) {
	msg := parseStreamLine(`{"type":"usage","prompt":"1200","completion":"300","total":"1500"}`)
	if msg.eventType != "usage" {
		t.Fatalf("event type = %q, want usage", msg.eventType)
	}
	if msg.usage == nil {
		t.Fatal("usage must be parsed")
	}
	want := &streamUsage{PromptTokens: 1200, CompletionTokens: 300, TotalTokens: 1500}
	if !reflect.DeepEqual(msg.usage, want) {
		t.Errorf("usage = %+v, want %+v", msg.usage, want)
	}

	// Events without a total carry no usage.
	if msg := parseStreamLine(`{"type":"content","text":"hi"}`); msg.usage != nil {
		t.Errorf("content event parsed usage: %+v", msg.usage)
	}
}

// TestToolDiffAttachesToToolBlock verifies a tool_diff event lands on the
// most recent tool block so the diff renders under "x tool called".
func TestToolDiffAttachesToToolBlock(t *testing.T) {
	m := tuiModel{}

	// The "tool" event renders the header; the "tool_diff" event carries
	// the diff in its "diff" field (not "text").
	m.handleStreamMsg(parseStreamLine(`{"type":"tool","name":"write_file","arguments":"{\"path\":\"a.txt\"}"}`))
	m.handleStreamMsg(parseStreamLine(`{"type":"tool_diff","diff":"@@ -1 +1 @@\n+hello\n"}`))

	if len(m.blocks) != 1 || m.blocks[0].kind != "tool" {
		t.Fatalf("expected one tool block, got %+v", m.blocks)
	}
	if got, want := m.blocks[0].diff, "@@ -1 +1 @@\n+hello\n"; got != want {
		t.Errorf("tool block diff = %q, want %q", got, want)
	}
}

func TestUsageString(t *testing.T) {
	m := tuiModel{selModel: "deepseek-v4-flash:cloud"}

	// No usage reported yet: nothing shown.
	if got := m.usageString(); got != "" {
		t.Errorf("empty usage rendered %q, want empty", got)
	}

	// 8.4K tokens in a 128K window is 6%.
	m.session.Usage = &streamUsage{PromptTokens: 8300, CompletionTokens: 100, TotalTokens: 8400}
	if got, want := m.usageString(), "8.4K (6%)"; got != want {
		t.Errorf("usageString = %q, want %q", got, want)
	}

	// Sub-1000 tokens render raw, rounding to 0%.
	m.session.Usage = &streamUsage{TotalTokens: 312}
	if got, want := m.usageString(), "312 (0%)"; got != want {
		t.Errorf("usageString = %q, want %q", got, want)
	}
}

// TestStreamingPreservesScrollback verifies that streamed content doesn't
// yank the viewport back to the bottom once the user has scrolled up,
// while the view stays pinned to the newest output while at the bottom.
func TestStreamingPreservesScrollback(t *testing.T) {
	m := &tuiModel{width: 100, height: 30, following: true, input: textarea.New()}
	m.layoutViewport()

	// A conversation long enough to scroll.
	for i := 0; i < 40; i++ {
		m.blocks = append(m.blocks, block{kind: "assistant", text: fmt.Sprintf("line %d", i)})
	}
	m.refreshViewport()
	if !m.viewport.AtBottom() {
		t.Fatal("viewport should start pinned to the bottom")
	}

	// Scrolling up (PgUp) detaches the view from the live tail.
	m.Update(tea.KeyPressMsg{Code: tea.KeyPgUp})
	if m.following || m.viewport.AtBottom() {
		t.Fatal("PgUp should detach the viewport from the bottom")
	}
	offset := m.viewport.YOffset()

	// New streamed content must not move the scrollback position.
	m.handleStreamMsg(parseStreamLine(`{"type":"content","text":" more"}`))
	m.refreshViewport()
	if got := m.viewport.YOffset(); got != offset {
		t.Fatalf("streamed content moved the scroll position: got %d, want %d", got, offset)
	}

	// End returns to the bottom and re-attaches; streaming keeps it pinned.
	m.Update(tea.KeyPressMsg{Code: tea.KeyEnd})
	if !m.following || !m.viewport.AtBottom() {
		t.Fatal("End should return to the bottom and re-attach")
	}
	m.handleStreamMsg(parseStreamLine(`{"type":"content","text":" more text"}`))
	m.refreshViewport()
	if !m.viewport.AtBottom() {
		t.Fatal("viewport should stay pinned to the newest output while at the bottom")
	}
}

// TestThinkingToggle verifies the /thinking command collapses reasoning
// blocks to a summary line in the rendered view without deleting them
// from the history, and that toggling again restores the full text.
func TestThinkingToggle(t *testing.T) {
	m := &tuiModel{width: 100, height: 30, input: textarea.New(), showReasoning: true}
	m.layoutViewport()
	m.blocks = []block{
		{kind: "reasoning", text: "thinking out loud", active: true},
		{kind: "assistant", text: "the answer"},
	}

	// Reasoning is visible by default.
	if got := ansi.Strip(m.renderBlocks(100)); !strings.Contains(got, "thinking out loud") {
		t.Fatal("reasoning should be visible by default")
	}

	// /thinking collapses reasoning but keeps the blocks in the history.
	m.handleInput("/thinking")
	if m.showReasoning {
		t.Fatal("/thinking should turn reasoning display off")
	}
	got := ansi.Strip(m.renderBlocks(100))
	if strings.Contains(got, "thinking out loud") {
		t.Fatal("reasoning text should be hidden after /thinking")
	}
	if !strings.Contains(got, "Thinking...") {
		t.Fatal("an in-progress reasoning block should render as Thinking...")
	}
	if !strings.Contains(got, "the answer") {
		t.Fatal("assistant content should stay visible")
	}
	if len(m.blocks) != 2 {
		t.Fatalf("/thinking must not delete blocks, got %d", len(m.blocks))
	}

	// Toggling again restores the full reasoning text.
	m.handleInput("/thinking")
	if !m.showReasoning {
		t.Fatal("/thinking should restore reasoning display")
	}
	if got := ansi.Strip(m.renderBlocks(100)); !strings.Contains(got, "thinking out loud") {
		t.Fatal("reasoning should be visible after toggling back")
	}
}

// TestReasoningCollapseLabel verifies the collapsed reasoning line:
// "Thinking..." while a block is streaming, "Thinking (Xs)" after it
// finishes, and plain "Thinking" for history-loaded blocks that have no
// measured duration.
func TestReasoningCollapseLabel(t *testing.T) {
	m := &tuiModel{width: 100, height: 30, input: textarea.New(), showReasoning: false}
	m.layoutViewport()

	// In progress: "Thinking...".
	m.handleStreamMsg(parseStreamLine(`{"type":"reasoning","text":"hmm"}`))
	if got := ansi.Strip(m.renderBlocks(100)); !strings.Contains(got, "Thinking...") {
		t.Fatalf("active reasoning should render Thinking..., got %q", got)
	}

	// reasoning_end finalizes the block with a measured duration.
	m.handleStreamMsg(parseStreamLine(`{"type":"reasoning_end"}`))
	got := ansi.Strip(m.renderBlocks(100))
	if strings.Contains(got, "Thinking...") {
		t.Fatal("reasoning should no longer be in progress after reasoning_end")
	}
	if !strings.Contains(got, "Thinking (") {
		t.Fatalf("finished reasoning should show a duration, got %q", got)
	}

	// A block loaded from session history has no duration: plain
	// "Thinking" (prefixed by a newline to exclude the "Thinking...")
	// and "(...)" variants).
	m.blocks = append(m.blocks, block{kind: "reasoning", text: "old thought"})
	got = ansi.Strip(m.renderBlocks(100))
	if !strings.Contains(got, "\nThinking\n") {
		t.Fatalf("history reasoning should render plain Thinking, got %q", got)
	}
}

// TestReloadPreservesReasoningDuration verifies the sequence that used to
// drop the timing: a turn finishes (the reasoning block carries its
// measured duration), the server persists and broadcasts "saved", and the
// TUI reloads the session. The reload rebuilds blocks from history, which
// has no duration, so without the restore step the collapsed
// "Thinking (5.0s)" line would fall back to plain "Thinking".
func TestReloadPreservesReasoningDuration(t *testing.T) {
	m := &tuiModel{width: 100, height: 30, input: textarea.New(), showReasoning: false}
	m.layoutViewport()
	m.session.ID = "s1"

	// The turn finished; the live blocks carry the measured duration.
	m.blocks = []block{
		{kind: "reasoning", text: "hmm, let me think", dur: 5 * time.Second},
		{kind: "assistant", text: "the answer"},
	}

	// The server saved the session; the TUI reloads it from history.
	m.Update(sessionLoadedMsg{session: Session{
		ID:    "s1",
		Model: "deepseek-v4-flash:cloud",
		Messages: []message{
			{Role: "assistant", Reasoning: "hmm, let me think", Content: "the answer"},
		},
	}})

	got := ansi.Strip(m.renderBlocks(100))
	if !strings.Contains(got, "Thinking (5.0s)") {
		t.Fatalf("reloaded reasoning should keep its duration, got %q", got)
	}
}

func TestCmdASelectsPromptOnly(t *testing.T) {
	m := &tuiModel{width: 100, height: 30, following: true, input: textarea.New()}
	m.layoutViewport()
	m.blocks = []block{{kind: "assistant", text: "conversation history stays put"}}
	m.refreshViewport()
	offset := m.viewport.YOffset()
	m.input.SetValue("hello prompt")

	m.Update(tea.KeyPressMsg{Code: 'a', Mod: tea.ModSuper})

	if !m.input.HasSelection() {
		t.Fatal("cmd+a should select the prompt")
	}
	if got := m.input.SelectedText(); got != "hello prompt" {
		t.Fatalf("selected %q, want the prompt", got)
	}
	if m.viewport.YOffset() != offset {
		t.Fatal("cmd+a must not move the conversation viewport")
	}
}

func TestIncrementalRenderMatchesFullRender(t *testing.T) {
	m := &tuiModel{width: 40, height: 30, following: true, input: textarea.New(), showReasoning: true}
	m.layoutViewport()
	for i := 0; i < 8; i++ {
		m.blocks = append(m.blocks, block{kind: "assistant", text: fmt.Sprintf("history block %d with enough words to wrap", i)})
	}
	m.refreshViewport()
	frozen := m.blocks[0].lines

	m.handleStreamMsg(parseStreamLine(`{"type":"content","text":"Streaming starts here and then "}`))
	m.refreshViewport()
	m.handleStreamMsg(parseStreamLine(`{"type":"content","text":"grows with more words that will wrap across lines."}`))
	m.refreshViewport()

	if len(frozen) == 0 || &frozen[0] != &m.blocks[0].lines[0] {
		t.Fatal("finalized history should keep its cached line slice")
	}
	got := strings.Join(m.contentLines, "\n")
	want := m.renderBlocks(m.innerWidth())
	if got != want {
		t.Fatalf("incremental lines != renderBlocks\n got: %q\nwant: %q", got, want)
	}
}

func TestResizeRewrapsCachedBlocks(t *testing.T) {
	m := &tuiModel{width: 80, height: 30, following: true, input: textarea.New()}
	m.layoutViewport()
	m.blocks = []block{{kind: "assistant", text: strings.Repeat("word ", 40)}}
	m.refreshViewport()
	wide := len(m.contentLines)

	m.Update(tea.WindowSizeMsg{Width: 20, Height: 30})
	if m.contentWidth != m.innerWidth() {
		t.Fatalf("contentWidth = %d, want %d", m.contentWidth, m.innerWidth())
	}
	if len(m.contentLines) <= wide {
		t.Fatalf("narrower wrap should produce more lines: wide=%d narrow=%d", wide, len(m.contentLines))
	}
}

func TestHandleInputCompact(t *testing.T) {
	m := tuiModel{session: SessionInfo{ID: "abc"}, input: textarea.New()}
	_, cmd := m.handleInput("/compact keep paths")
	if !m.streaming {
		t.Fatal("idle /compact should start a stream")
	}
	if cmd == nil {
		t.Fatal("expected sendCmd")
	}

	m = tuiModel{input: textarea.New()}
	m.handleInput("/compact")
	if m.errMsg != "no session to compact" {
		t.Fatalf("errMsg = %q", m.errMsg)
	}

	m = tuiModel{session: SessionInfo{ID: "abc"}, streaming: true, input: textarea.New()}
	_, cmd = m.handleInput("/compact")
	if m.errMsg != "" {
		t.Fatalf("mid-stream /compact should not error, got %q", m.errMsg)
	}
	if cmd == nil {
		t.Fatal("expected compactCmd")
	}

	matches := matchCommands("/comp")
	if len(matches) != 1 || matches[0].name != "/compact" {
		t.Fatalf("matchCommands(/comp) = %+v", matches)
	}
}

func TestCompactionStreamUI(t *testing.T) {
	m := tuiModel{width: 100, height: 30, input: textarea.New(), showReasoning: true}
	m.layoutViewport()
	m.handleStreamMsg(parseStreamLine(`{"type":"compaction"}`))
	got := ansi.Strip(m.renderBlocks(100))
	if !strings.Contains(got, "Compacting...") {
		t.Fatalf("active compact: %q", got)
	}
	m.handleStreamMsg(parseStreamLine(`{"type":"compaction_end","text":"Previous conversation summary:\n\n## Goal\nShip it"}`))
	got = ansi.Strip(m.renderBlocks(100))
	if strings.Contains(got, "Compacting...") {
		t.Fatalf("still active after end: %q", got)
	}
	if !strings.Contains(got, "Compacted (") {
		t.Fatalf("expected Compacted duration, got %q", got)
	}
	if !strings.Contains(got, "Previous conversation summary") || !strings.Contains(got, "## Goal") || !strings.Contains(got, "Ship it") {
		t.Fatalf("summary payload should render as model output, got %q", got)
	}
}

func TestSessionToBlocksShowsStoredSummary(t *testing.T) {
	blocks := sessionToBlocks(Session{
		Messages: []message{
			{Role: "user", Content: "hi"},
			{Role: "tool", Content: "ok\tgochat\t0.444s"},
		},
		CompactionSummary: "## Goal\nShip compaction",
	})
	got := ""
	for _, b := range blocks {
		if b.kind == "compaction" {
			got = b.text
		}
	}
	want := compactionPromptText("## Goal\nShip compaction")
	if got != want {
		t.Fatalf("compaction block text = %q, want %q", got, want)
	}
	if blocks[len(blocks)-1].kind != "compaction" {
		t.Fatalf("summary should be the last block so the viewport shows it, got %+v", blocks[len(blocks)-1])
	}

	dup := sessionToBlocks(Session{
		Messages: []message{
			{Role: "compaction", Content: want},
		},
		CompactionSummary: "## Goal\nShip compaction",
	})
	n := 0
	for _, b := range dup {
		if b.kind == "compaction" {
			n++
		}
	}
	if n != 1 {
		t.Fatalf("stored compaction message should not be duplicated, got %d", n)
	}
}

func TestImageChipMatchesMarkerWidth(t *testing.T) {
	for i := 1; i <= 12; i++ {
		mark := imageMarker(i)
		chip := imageChip(i)
		if lipgloss.Width(mark) != lipgloss.Width(chip) {
			t.Errorf("n=%d marker width %d chip width %d (%q vs %q)",
				i, lipgloss.Width(mark), lipgloss.Width(chip), mark, chip)
		}
	}
}
