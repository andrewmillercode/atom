// Conversation compaction folds older session turns into a short summary
// once the last reported prompt is large enough. The TUI still shows the
// full Messages transcript; only the context sent to the model shrinks.
package main

import (
	"bytes"
	"context"
	"encoding/json"
	"fmt"
	"io"
	"net/http"
	"os"
	"strings"
	"time"
)

// compactionTokenThreshold is the last-reported prompt size that triggers
// a compact. Equal to the threshold is not enough: we wait until the
// session has clearly grown past it.
const compactionTokenThreshold = 150_000

// compactionModelID is always used for summaries, independent of the
// session's current chat model.
const compactionModelID = "deepseek-v4-flash:0731"

// compactionToolResultLimit caps how much of a tool result is serialized
// into the summary prompt so one huge read cannot dominate the request.
const compactionToolResultLimit = 2000

const compactionSummaryPreamble = "Previous conversation summary:\n\n"
const compactionSummaryAck = "Understood. I will continue from this summary."

// errNothingToCompact is returned when every message is already folded
// (or the only leftover is a trailing user turn kept for the next send).
var errNothingToCompact = fmt.Errorf("nothing to compact")

// compactionProviderOverride, when set, replaces compactionProvider.
// Tests use it to point at an httptest server instead of Ollama.
var compactionProviderOverride func() (baseURL, key string)

// compactionSystemPrompt asks the summarizer for a structured brief, not
// a reply that continues the conversation.
const compactionSystemPrompt = `You are compacting a coding-agent conversation into a structured summary. Do not continue the conversation or answer the user. Produce only the summary.

Use this outline:

## Goal
## Constraints & Preferences
## Progress (Done / In Progress / Blocked)
## Key Decisions
## Next Steps
## Critical Context

Preserve the user's latest intent and any critical file paths, decisions, and unfinished work. If a previous summary is included, update it rather than discarding it.`

// shouldCompact reports whether the session's last provider usage is
// large enough to fold. A nil Usage means we have no signal yet, so we
// must not compact.
func shouldCompact(sess *Session) bool {
	if sess == nil || sess.Usage == nil {
		return false
	}
	return sess.Usage.PromptTokens > compactionTokenThreshold
}

// compactionProvider picks the Ollama endpoint used only for compaction.
// A cloud key talks to ollama.com; otherwise we use the local daemon.
func compactionProvider() (baseURL, key string) {
	if compactionProviderOverride != nil {
		return compactionProviderOverride()
	}
	key = os.Getenv("OLLAMA_API_KEY")
	if key == "" {
		key = loadProviderKey("ollama-cloud")
	}
	if key != "" {
		return "https://ollama.com/v1", key
	}
	return "http://localhost:11434/v1", ""
}

// clampIndex keeps i inside [0, n] so CompactedThrough cannot panic
// after a session is edited or loaded with a stale index.
func clampIndex(i, n int) int {
	if i < 0 {
		return 0
	}
	if i > n {
		return n
	}
	return i
}

// compactSpan is the half-open range of Messages that should be folded.
// A trailing user message is left out so the current question is sent
// verbatim after the summary.
func compactSpan(sess *Session) (start, end int, ok bool) {
	n := len(sess.Messages)
	start = clampIndex(sess.CompactedThrough, n)
	end = n
	for end > start {
		role := sess.Messages[end-1].Role
		if role == "compaction" {
			end--
			continue
		}
		if role == "user" {
			end--
		}
		break
	}
	if start >= end {
		return start, end, false
	}
	return start, end, true
}

// llmMessages builds the context for a model request: fresh instructions,
// an optional compaction brief, then only the unsummarized tail.
func llmMessages(sess *Session) []message {
	msgs := append([]message{}, sess.Instructions...)
	if sess.CompactionSummary != "" {
		msgs = append(msgs,
			message{Role: "user", Content: compactionSummaryPreamble + sess.CompactionSummary},
			message{Role: "assistant", Content: compactionSummaryAck},
		)
	}
	start := clampIndex(sess.CompactedThrough, len(sess.Messages))
	for _, m := range sess.Messages[start:] {
		if m.Role == "compaction" {
			continue
		}
		msgs = append(msgs, m)
	}
	return sanitizeMessages(msgs)
}

// serializeConversation renders history as labeled prose so the
// summarizer does not treat it as a live chat to continue.
func serializeConversation(msgs []message, previousSummary string) string {
	var b strings.Builder
	if previousSummary != "" {
		b.WriteString("Previous summary:\n")
		b.WriteString(previousSummary)
		b.WriteString("\n\n")
	}
	for _, m := range msgs {
		switch m.Role {
		case "compaction":
			continue
		case "user":
			if text := serializeText(m.Content, m.Images); text != "" {
				b.WriteString("[User]: ")
				b.WriteString(text)
				b.WriteString("\n")
			}
		case "assistant":
			if m.Reasoning != "" {
				b.WriteString("[Assistant thinking]: ")
				b.WriteString(m.Reasoning)
				b.WriteString("\n")
			}
			if m.Content != "" {
				b.WriteString("[Assistant]: ")
				b.WriteString(m.Content)
				b.WriteString("\n")
			}
			if len(m.ToolCalls) > 0 {
				parts := make([]string, 0, len(m.ToolCalls))
				for _, tc := range m.ToolCalls {
					parts = append(parts, tc.Function.Name+"("+tc.Function.Arguments+")")
				}
				b.WriteString("[Assistant tool calls]: ")
				b.WriteString(strings.Join(parts, "; "))
				b.WriteString("\n")
			}
		case "tool":
			if text := serializeText(truncateToolResult(m.Content), m.Images); text != "" {
				b.WriteString("[Tool result]: ")
				b.WriteString(text)
				b.WriteString("\n")
			}
		}
	}
	return strings.TrimSpace(b.String())
}

// serializeText joins message text with image placeholders. Empty
// sections are omitted so the summary prompt stays compact.
func serializeText(content string, images []imageData) string {
	var parts []string
	if content != "" {
		parts = append(parts, content)
	}
	for range images {
		parts = append(parts, "[image attached]")
	}
	return strings.Join(parts, "\n")
}

func truncateToolResult(s string) string {
	if len(s) <= compactionToolResultLimit {
		return s
	}
	omitted := len(s) - compactionToolResultLimit
	return s[:compactionToolResultLimit] + fmt.Sprintf("\n...[%d characters omitted]", omitted)
}

// compactSession summarizes Messages[start:end] and stores the brief on
// sess. The last user message, if any, stays in the live tail. On
// failure the session is left unchanged so the turn can continue.
func compactSession(ctx context.Context, sess *Session, client *http.Client, baseURL, key, extra string) error {
	start, end, ok := compactSpan(sess)
	if !ok {
		return errNothingToCompact
	}
	body := serializeConversation(sess.Messages[start:end], sess.CompactionSummary)
	if extra = strings.TrimSpace(extra); extra != "" {
		body += "\n\nAdditional instructions from the user:\n" + extra
	}
	if body == "" {
		return errNothingToCompact
	}
	if client == nil {
		client = &http.Client{Timeout: 10 * time.Minute}
	}
	baseURL = strings.TrimSuffix(baseURL, "/")

	reqBody, err := json.Marshal(chatRequest{
		Model:           compactionModelID,
		Messages:        []message{{Role: "system", Content: compactionSystemPrompt}, {Role: "user", Content: body}},
		Stream:          false,
		ReasoningEffort: "none",
	})
	if err != nil {
		return err
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, baseURL+"/chat/completions", bytes.NewReader(reqBody))
	if err != nil {
		return err
	}
	req.Header.Set("Content-Type", "application/json")
	req.Header.Set("Authorization", "Bearer "+key)

	resp, err := client.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	raw, err := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if err != nil {
		return err
	}
	if resp.StatusCode != http.StatusOK {
		return fmt.Errorf("%s: %s", resp.Status, strings.TrimSpace(string(raw)))
	}

	var parsed struct {
		Choices []struct {
			Message struct {
				Content          json.RawMessage `json:"content"`
				Reasoning        string          `json:"reasoning"`
				ReasoningContent string          `json:"reasoning_content"`
			} `json:"message"`
		} `json:"choices"`
		Usage *streamUsage `json:"usage"`
	}
	if err := json.Unmarshal(raw, &parsed); err != nil {
		return err
	}
	if len(parsed.Choices) == 0 {
		return fmt.Errorf("compaction response had no choices")
	}
	msg := parsed.Choices[0].Message
	summary := compactChoiceText(msg.Content, msg.Reasoning, msg.ReasoningContent)
	if summary == "" {
		return fmt.Errorf("compaction response was empty")
	}

	prompt := compactionPromptText(summary)
	entry := message{
		Role:     "compaction",
		Content:  prompt,
		Provider: providerNameForURL(baseURL),
		Model:    compactionModelID,
		Usage:    parsed.Usage,
	}
	// Append the brief at the end so the TUI shows it as the latest
	// model output. CompactedThrough still points at the live tail;
	// llmMessages skips role=compaction so this display copy is not
	// sent twice.
	sess.Messages = append(sess.Messages, entry)
	sess.CompactionSummary = summary
	sess.CompactedThrough = end
	sess.Usage = estimateSessionUsage(sess)
	return nil
}

// compactionPromptText is the user-side payload llmMessages sends after
// a fold: the brief, without system instructions or the assistant ack.
func compactionPromptText(summary string) string {
	return compactionSummaryPreamble + summary
}

// compactChoiceText pulls the summary out of a chat-completions choice.
// Flash models often put the brief in reasoning when content is empty,
// and some routers send content as a part array instead of a string.
func compactChoiceText(content json.RawMessage, reasoning, reasoningContent string) string {
	if s := strings.TrimSpace(contentText(content)); s != "" {
		return s
	}
	if s := strings.TrimSpace(reasoning); s != "" {
		return s
	}
	return strings.TrimSpace(reasoningContent)
}

func contentText(raw json.RawMessage) string {
	if len(raw) == 0 || string(raw) == "null" {
		return ""
	}
	var s string
	if json.Unmarshal(raw, &s) == nil {
		return s
	}
	var parts []struct {
		Type string `json:"type"`
		Text string `json:"text"`
	}
	if json.Unmarshal(raw, &parts) != nil {
		return ""
	}
	var b strings.Builder
	for _, p := range parts {
		if p.Type == "text" || p.Type == "" {
			b.WriteString(p.Text)
		}
	}
	return b.String()
}

// estimateSessionUsage approximates the tokens the next chat request
// will send: instructions, the compaction brief, and the live tail.
// Providers don't report usage for a compact itself in a form that
// matches the rebuilt context, so the status-bar meter uses this until
// the next real chat round.
func estimateSessionUsage(sess *Session) *streamUsage {
	n := 0
	for _, m := range llmMessages(sess) {
		n += len(m.Content) + len(m.Reasoning)
		for _, tc := range m.ToolCalls {
			n += len(tc.Function.Name) + len(tc.Function.Arguments)
		}
	}
	tok := (n + 3) / 4
	if tok < 1 {
		tok = 1
	}
	return &streamUsage{PromptTokens: tok, TotalTokens: tok}
}

// handleCompact folds the session on demand from /compact. Unlike the
// auto path, it ignores the token threshold. Optional JSON field
// "instructions" is forwarded to the summarizer as extra focus.
func handleCompact(store *SessionStore, w http.ResponseWriter, r *http.Request, id string) {
	sess := store.Get(id)
	if sess == nil {
		http.Error(w, "session not found", http.StatusNotFound)
		return
	}

	var body struct {
		Instructions string `json:"instructions"`
	}
	if r.Body != nil {
		json.NewDecoder(r.Body).Decode(&body)
	}

	// Mid-turn: interrupt the current model request so handleSend can
	// fold and resume. The TUI watches compaction events on /send.
	if requestSessionCompact(id, body.Instructions) {
		w.WriteHeader(http.StatusNoContent)
		return
	}

	if _, _, ok := compactSpan(sess); !ok {
		http.Error(w, errNothingToCompact.Error(), http.StatusBadRequest)
		return
	}

	baseURL, key := compactionProvider()
	if err := compactSession(r.Context(), sess, nil, baseURL, key, body.Instructions); err != nil {
		if err == errNothingToCompact {
			http.Error(w, err.Error(), http.StatusBadRequest)
			return
		}
		http.Error(w, err.Error(), http.StatusBadGateway)
		return
	}
	persistSession(store, sess, id)
	writeJSON(w, map[string]any{
		"summary":           sess.CompactionSummary,
		"compacted_through": sess.CompactedThrough,
	})
}
