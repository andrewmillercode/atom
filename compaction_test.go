package main

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
)

func TestShouldCompact(t *testing.T) {
	tests := []struct {
		name string
		sess *Session
		want bool
	}{
		{name: "nil usage", sess: &Session{}, want: false},
		{name: "below threshold", sess: &Session{Usage: &streamUsage{PromptTokens: 1000}}, want: false},
		{name: "equal threshold", sess: &Session{Usage: &streamUsage{PromptTokens: 150000}}, want: false},
		{name: "above threshold", sess: &Session{Usage: &streamUsage{PromptTokens: 150001}}, want: true},
	}
	for _, tt := range tests {
		t.Run(tt.name, func(t *testing.T) {
			if got := shouldCompact(tt.sess); got != tt.want {
				t.Fatalf("shouldCompact() = %v, want %v", got, tt.want)
			}
		})
	}
}

func TestSerializeConversation(t *testing.T) {
	longTool := strings.Repeat("x", compactionToolResultLimit+25)
	var bash toolCall
	bash.Function.Name = "bash"
	bash.Function.Arguments = `{"command":"ls"}`

	got := serializeConversation([]message{
		{Role: "user", Content: "look at this", Images: []imageData{{MIME: "image/png", Data: "abc"}}},
		{Role: "assistant", Reasoning: "plan", Content: "calling ls", ToolCalls: []toolCall{bash}},
		{Role: "tool", Content: longTool},
		{Role: "assistant", Content: ""}, // empty sections skipped
	}, "older brief")

	if !strings.Contains(got, "Previous summary:\nolder brief") {
		t.Fatalf("missing previous summary:\n%s", got)
	}
	if !strings.Contains(got, "[User]: look at this\n[image attached]") {
		t.Fatalf("user/image serialization:\n%s", got)
	}
	if !strings.Contains(got, "[Assistant thinking]: plan") {
		t.Fatalf("missing thinking:\n%s", got)
	}
	if !strings.Contains(got, "[Assistant]: calling ls") {
		t.Fatalf("missing assistant text:\n%s", got)
	}
	if !strings.Contains(got, `[Assistant tool calls]: bash({"command":"ls"})`) {
		t.Fatalf("missing tool calls:\n%s", got)
	}
	if !strings.Contains(got, "[Tool result]: "+strings.Repeat("x", compactionToolResultLimit)) {
		t.Fatalf("truncated tool prefix missing:\n%s", got)
	}
	if !strings.Contains(got, "...[25 characters omitted]") {
		t.Fatalf("omitted-length marker missing:\n%s", got)
	}
	if strings.Contains(got, "[Assistant]: \n") {
		t.Fatalf("empty assistant section should be skipped:\n%s", got)
	}
}

func TestLlmMessages(t *testing.T) {
	instr := []message{{Role: "system", Content: "be helpful"}}
	tail := []message{
		{Role: "user", Content: "one"},
		{Role: "assistant", Content: "two"},
		{Role: "user", Content: "three"},
	}

	t.Run("no summary", func(t *testing.T) {
		got := llmMessages(&Session{Instructions: instr, Messages: tail})
		if len(got) != 4 {
			t.Fatalf("len = %d, want 4: %+v", len(got), got)
		}
		if got[0].Content != "be helpful" || got[1].Content != "one" {
			t.Fatalf("unexpected: %+v", got)
		}
	})

	t.Run("summary and CompactedThrough", func(t *testing.T) {
		got := llmMessages(&Session{
			Instructions:      instr,
			Messages:          tail,
			CompactionSummary: "brief",
			CompactedThrough:  2,
		})
		if len(got) != 4 {
			t.Fatalf("len = %d, want 4: %+v", len(got), got)
		}
		if got[1].Role != "user" || !strings.Contains(got[1].Content, "brief") {
			t.Fatalf("summary user: %+v", got[1])
		}
		if got[2].Role != "assistant" || got[2].Content != compactionSummaryAck {
			t.Fatalf("ack: %+v", got[2])
		}
		if got[3].Content != "three" {
			t.Fatalf("tail: %+v", got[3])
		}
	})

	t.Run("skips display compaction message", func(t *testing.T) {
		msgs := append(append([]message{}, tail...), message{
			Role:    "compaction",
			Content: compactionPromptText("brief"),
		})
		got := llmMessages(&Session{
			Instructions:      instr,
			Messages:          msgs,
			CompactionSummary: "brief",
			CompactedThrough:  2,
		})
		for _, m := range got {
			if m.Role == "compaction" {
				t.Fatalf("display compaction leaked into llm payload: %+v", got)
			}
		}
		if got[len(got)-1].Content != "three" {
			t.Fatalf("live tail lost: %+v", got)
		}
	})

	t.Run("clamp CompactedThrough", func(t *testing.T) {
		got := llmMessages(&Session{
			Instructions:      instr,
			Messages:          tail,
			CompactionSummary: "brief",
			CompactedThrough:  99,
		})
		if len(got) != 3 {
			t.Fatalf("len = %d, want 3 (instructions+summary+ack): %+v", len(got), got)
		}
	})

	t.Run("last user kept after compact", func(t *testing.T) {
		sess := &Session{
			Instructions:      instr,
			Messages:          tail,
			CompactionSummary: "folded one and two",
			CompactedThrough:  2,
		}
		got := llmMessages(sess)
		if got[len(got)-1].Role != "user" || got[len(got)-1].Content != "three" {
			t.Fatalf("last live message: %+v", got[len(got)-1])
		}
		if len(sess.Messages) != 3 {
			t.Fatalf("full transcript must stay on the session, got %d", len(sess.Messages))
		}
	})
}

func TestCompactSessionSuccess(t *testing.T) {
	var got chatRequest
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/chat/completions" {
			t.Errorf("path = %s", r.URL.Path)
		}
		if err := json.NewDecoder(r.Body).Decode(&got); err != nil {
			t.Errorf("decode request: %v", err)
		}
		w.Header().Set("Content-Type", "application/json")
		json.NewEncoder(w).Encode(map[string]any{
			"choices": []map[string]any{
				{"message": map[string]any{"content": "## Goal\nShip compaction"}},
			},
		})
	}))
	defer srv.Close()

	sess := &Session{
		Messages: []message{
			{Role: "user", Content: "hello"},
			{Role: "assistant", Content: "hi there"},
			{Role: "user", Content: "what next?"},
		},
		Usage: &streamUsage{PromptTokens: 150001},
	}
	if err := compactSession(context.Background(), sess, srv.Client(), srv.URL, "test-key", ""); err != nil {
		t.Fatalf("compactSession: %v", err)
	}
	if got.Model != compactionModelID {
		t.Fatalf("model = %q, want %q", got.Model, compactionModelID)
	}
	if got.Stream {
		t.Fatal("compaction request must not stream")
	}
	if len(got.Messages) < 2 || !strings.Contains(got.Messages[1].Content, "[User]: hello") {
		t.Fatalf("serialized history missing from request: %+v", got.Messages)
	}
	if !strings.Contains(got.Messages[1].Content, "[Assistant]: hi there") {
		t.Fatalf("assistant turn missing from request: %+v", got.Messages)
	}
	if sess.CompactionSummary != "## Goal\nShip compaction" {
		t.Fatalf("summary = %q", sess.CompactionSummary)
	}
	if sess.CompactedThrough != 2 {
		t.Fatalf("CompactedThrough = %d, want 2 (last user kept)", sess.CompactedThrough)
	}
	if len(sess.Messages) != 4 {
		t.Fatalf("len(Messages) = %d, want 4 after appending summary", len(sess.Messages))
	}
	last := sess.Messages[len(sess.Messages)-1]
	if last.Role != "compaction" || last.Content != compactionPromptText("## Goal\nShip compaction") {
		t.Fatalf("appended summary message: %+v", last)
	}
	if sess.Usage == nil || sess.Usage.TotalTokens <= 0 {
		t.Fatalf("Usage should reflect compacted context, got %+v", sess.Usage)
	}
	live := llmMessages(sess)
	if live[len(live)-1].Content != "what next?" {
		t.Fatalf("live tail should keep the last user message, got %+v", live)
	}
	for _, m := range live {
		if m.Content == "hi there" {
			t.Fatal("folded assistant text should not be in live llm context")
		}
	}
}

func TestCompactSessionFailureLeavesSessionUnchanged(t *testing.T) {
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		http.Error(w, "boom", http.StatusInternalServerError)
	}))
	defer srv.Close()

	usage := &streamUsage{PromptTokens: 150001}
	sess := &Session{
		Messages: []message{
			{Role: "user", Content: "hello"},
			{Role: "assistant", Content: "hi"},
			{Role: "user", Content: "again"},
		},
		Usage:             usage,
		CompactionSummary: "old brief",
		CompactedThrough:  1,
	}
	if err := compactSession(context.Background(), sess, srv.Client(), srv.URL, "", ""); err == nil {
		t.Fatal("expected error from 500")
	}
	if sess.CompactionSummary != "old brief" || sess.CompactedThrough != 1 {
		t.Fatalf("session mutated: summary=%q through=%d", sess.CompactionSummary, sess.CompactedThrough)
	}
	if len(sess.Messages) != 3 {
		t.Fatalf("Messages mutated, len=%d", len(sess.Messages))
	}
	if sess.Usage != usage || sess.Usage.PromptTokens != 150001 {
		t.Fatalf("Usage mutated: %+v", sess.Usage)
	}
}

func TestCompactSessionExtraInstructions(t *testing.T) {
	var got chatRequest
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if err := json.NewDecoder(r.Body).Decode(&got); err != nil {
			t.Errorf("decode: %v", err)
		}
		json.NewEncoder(w).Encode(map[string]any{
			"choices": []map[string]any{
				{"message": map[string]any{"content": "focused brief"}},
			},
		})
	}))
	defer srv.Close()

	sess := &Session{
		Messages: []message{
			{Role: "user", Content: "hello"},
			{Role: "assistant", Content: "hi"},
		},
	}
	if err := compactSession(context.Background(), sess, srv.Client(), srv.URL, "", "keep the file paths"); err != nil {
		t.Fatalf("compactSession: %v", err)
	}
	if len(got.Messages) < 2 || !strings.Contains(got.Messages[1].Content, "Additional instructions from the user:\nkeep the file paths") {
		t.Fatalf("extra instructions missing: %+v", got.Messages)
	}
}

func TestCompactSessionNothingToCompact(t *testing.T) {
	sess := &Session{Messages: []message{{Role: "user", Content: "only"}}}
	err := compactSession(context.Background(), sess, nil, "http://unused", "", "")
	if err != errNothingToCompact {
		t.Fatalf("err = %v, want errNothingToCompact", err)
	}
}

func TestHandleCompact(t *testing.T) {
	isolateDataDir(t)
	store, err := NewSessionStore()
	if err != nil {
		t.Fatalf("store: %v", err)
	}

	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		json.NewEncoder(w).Encode(map[string]any{
			"choices": []map[string]any{
				{"message": map[string]any{"content": "manual brief"}},
			},
		})
	}))
	defer srv.Close()
	compactionProviderOverride = func() (string, string) { return srv.URL, "k" }
	t.Cleanup(func() { compactionProviderOverride = nil })

	t.Run("not found", func(t *testing.T) {
		rec := httptest.NewRecorder()
		req := httptest.NewRequest(http.MethodPost, "/api/sessions/missing/compact", nil)
		handleCompact(store, rec, req, "missing")
		if rec.Code != http.StatusNotFound {
			t.Fatalf("status = %d", rec.Code)
		}
	})

	t.Run("nothing to compact", func(t *testing.T) {
		sess := store.Create("m", "/tmp", nil)
		rec := httptest.NewRecorder()
		req := httptest.NewRequest(http.MethodPost, "/compact", nil)
		handleCompact(store, rec, req, sess.ID)
		if rec.Code != http.StatusBadRequest {
			t.Fatalf("status = %d body %s", rec.Code, rec.Body.String())
		}
	})

	t.Run("in progress", func(t *testing.T) {
		sess := store.Create("m", "/tmp", nil)
		sess.Messages = []message{
			{Role: "user", Content: "a"},
			{Role: "assistant", Content: "b"},
		}
		turn, _ := startTurn(sess.ID, "t1", context.Background())
		defer endTurn(sess.ID, turn)
		rec := httptest.NewRecorder()
		req := httptest.NewRequest(http.MethodPost, "/compact", strings.NewReader(`{"instructions":"focus"}`))
		handleCompact(store, rec, req, sess.ID)
		if rec.Code != http.StatusNoContent {
			t.Fatalf("status = %d", rec.Code)
		}
		extra, ok := turn.takeCompact()
		if !ok || extra != "focus" {
			t.Fatalf("queued compact = (%q, %v)", extra, ok)
		}
	})

	t.Run("success", func(t *testing.T) {
		sess := store.Create("m", "/tmp", nil)
		sess.Messages = []message{
			{Role: "user", Content: "a"},
			{Role: "assistant", Content: "b"},
			{Role: "user", Content: "c"},
		}
		body, _ := json.Marshal(map[string]string{"instructions": "focus on tests"})
		rec := httptest.NewRecorder()
		req := httptest.NewRequest(http.MethodPost, "/compact", strings.NewReader(string(body)))
		handleCompact(store, rec, req, sess.ID)
		if rec.Code != http.StatusOK {
			t.Fatalf("status = %d body %s", rec.Code, rec.Body.String())
		}
		got := store.Get(sess.ID)
		if got.CompactionSummary != "manual brief" {
			t.Fatalf("summary = %q", got.CompactionSummary)
		}
		if got.CompactedThrough != 2 {
			t.Fatalf("CompactedThrough = %d, want 2", got.CompactedThrough)
		}
		last := got.Messages[len(got.Messages)-1]
		if last.Role != "compaction" {
			t.Fatalf("last message role = %q, want compaction", last.Role)
		}
	})
}

func TestQueueCompactCancelsRound(t *testing.T) {
	turn, turnCtx := startTurn("sid", "tid", context.Background())
	defer endTurn("sid", turn)
	roundCtx, cancel := context.WithCancel(turnCtx)
	turn.setRoundCancel(cancel)
	turn.queueCompact("focus")
	if roundCtx.Err() == nil {
		t.Fatal("queued compact should cancel the provider round")
	}
	if turnCtx.Err() != nil {
		t.Fatal("queued compact must not cancel the whole turn")
	}
	extra, ok := turn.takeCompact()
	if !ok || extra != "focus" {
		t.Fatalf("takeCompact = (%q, %v)", extra, ok)
	}
}

func TestCompactChoiceText(t *testing.T) {
	if got := compactChoiceText([]byte(`"brief"`), "think", ""); got != "brief" {
		t.Fatalf("string content: %q", got)
	}
	if got := compactChoiceText([]byte(`[{"type":"text","text":"from parts"}]`), "", ""); got != "from parts" {
		t.Fatalf("parts: %q", got)
	}
	if got := compactChoiceText(nil, "  in reasoning  ", ""); got != "in reasoning" {
		t.Fatalf("reasoning fallback: %q", got)
	}
	if got := compactChoiceText([]byte("null"), "", "alt"); got != "alt" {
		t.Fatalf("reasoning_content fallback: %q", got)
	}
}

func TestSerializeSkipsCompactionRole(t *testing.T) {
	got := serializeConversation([]message{
		{Role: "user", Content: "hi"},
		{Role: "compaction", Content: compactionPromptText("old")},
		{Role: "assistant", Content: "ok"},
	}, "")
	if strings.Contains(got, "Previous conversation summary") || strings.Contains(got, "old") {
		t.Fatalf("compaction payload leaked into summarizer input:\n%s", got)
	}
	if !strings.Contains(got, "[User]: hi") || !strings.Contains(got, "[Assistant]: ok") {
		t.Fatalf("expected remaining turns:\n%s", got)
	}
}
