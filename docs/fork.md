# `/fork` — fork session from a user message

Mirrors OpenCode's UX: a `/fork` slash command opens a picker showing the
session header and every user message with timestamp. Selecting a row creates a
new session with the transcript copied up to (and excluding) that message, then
pre-fills the chosen message's text into the new session's prompt input.
Selecting the session header (the "fork from latest" row) copies the entire
transcript and leaves the draft empty. Fork is conversation-only — no file
isolation, matching OpenCode's behavior.

## UX

```
┌─ Fork session — type to filter, ↑↓ to navigate, Enter to fork, Esc to cancel ─┐
│                                                                                │
│ > query                                                                          │
│                                                                                │
│ ── Session ──                                                                   │
│   ▸ Migrate Salesforce loads from Matillion                  sonnet-4 · 14 msg  │
│                                                                                │
│ ── User messages ──                                                             │
│     14:02  Convert the loader into a streaming endpoint                       │
│     14:11  Now extract the auth header into its own middleware                │
│     14:30  Add a /retry route that surfaces provider errors                   │
│     15:01  Make the retry honour exponential backoff                          │
│                                                                                │
│                                       4/42 user messages                       │
└────────────────────────────────────────────────────────────────────────────────┘
```

- Default selection is the "session" row (fork from latest).
- Search filters user-message rows by case-insensitive substring match; the
  session row stays pinned at the top and is not filtered.
- Enter / click confirms. Esc closes.
- New child session is created server-side; the TUI switches to it and writes
  the draft into the prompt input.

## Data model ([`crates/atom-core/src/types.rs`](crates/atom-core/src/types.rs))

Add an optional per-message timestamp so the picker can show one and survive
restarts:

```rust
pub struct Message {
    // ... existing fields ...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
}
```

Stored as part of the JSON in `session_messages.message TEXT`
([`crates/atom-core/src/session/store.rs:298`](crates/atom-core/src/session/store.rs#L298))
— additive, no schema migration. Old messages load with `None`; the renderer
falls back to `—`. New messages get `Some(Utc::now())` set at write time in
[`crates/atom-server/src/turn.rs`](crates/atom-server/src/turn.rs) wherever
`Message` is constructed today.

`Session` already has `parent_id`
([`store.rs:89`](crates/atom-core/src/session/store.rs#L89)) and `create_child()`
([`store.rs:366`](crates/atom-core/src/session/store.rs#L366)), so lineage is
already modeled — we just need to surface it.

## Server: `POST /api/sessions/{id}/fork`

Add a new arm to the route table in
[`crates/atom-server/src/http.rs`](crates/atom-server/src/http.rs) near
[`http.rs:257`](crates/atom-server/src/http.rs#L257):

```rust
"fork" => {
    if method != "POST" {
        return error_resp(StatusCode::METHOD_NOT_ALLOWED, "method not allowed");
    }
    handle_fork(state, req, id).await
}
```

`handle_fork`:

1. Decode body `{ message_id?: string }`. `None` → fork from latest.
2. Load source via `store.get(id)`. 404 if missing.
3. If `message_id` is set, validate it's a `role == "user"` message in that
   session. 400 otherwise (this keeps the API surface honest even though the
   TUI only shows user messages).
4. Build child `Session` with `messages = source.messages[..idx]`
   (`idx = message_position` for fork-from-message, `idx = source.messages.len()`
   for latest). Same model, provider, cwd, thinking as source. `parent_id =
   source.id`. Fresh id via `new_session_id()`. `status = DelegateStatus::Done`
   (not Queued — this is a user-owned session, not a dispatch child).
5. `sess.title = format!("{} (fork #1)", source.title);` — simple suffix, no
   sibling-aware increment.
6. Persist via `store.save(&sess)`, install into `index`.
7. Return `{ info: SessionInfo, draft: message.content }` (draft is `""` for
   fork-from-latest, otherwise the selected message's content). 200.

The child title pattern matches OpenCode. Two forks of the same session share
a title — known wart, deferrable.

## Client wrapper ([`crates/atom-tui/src/api.rs`](crates/atom-tui/src/api.rs))

```rust
pub struct ForkedSession {
    pub info: SessionInfo,
    pub draft: String,
}

pub async fn fork_session(
    source_id: &str,
    message_id: Option<&str>,
) -> Result<ForkedSession> {
    let body = json!({ "message_id": message_id });
    let v = atom_server::client::post(
        &format!("/api/sessions/{source_id}/fork"),
        &body,
    ).await?;
    Ok(serde_json::from_value(v)?)
}
```

## TUI changes

### Slash registry ([`crates/atom-tui/src/overlays.rs`](crates/atom-tui/src/overlays.rs))

- Add `Fork` to `OverlayKind` ([`overlays.rs:20`](crates/atom-tui/src/overlays.rs#L20)).
- Add to `COMMANDS` ([`overlays.rs:58`](crates/atom-tui/src/overlays.rs#L58)):
  ```rust
  Command { name: "/fork", desc: "fork this session from a user message", kind: "" },
  ```
- Add to `DEFAULT_COMMANDS` ([`overlays.rs:227`](crates/atom-tui/src/overlays.rs#L227)).
- Add a `Fork` arm to `overlay_title()`
  ([`overlays.rs:1113`](crates/atom-tui/src/overlays.rs#L1113)),
  `overlay_count()` ([`overlays.rs:376`](crates/atom-tui/src/overlays.rs#L376)),
  and `overlay_has_query()`
  ([`overlays.rs:365`](crates/atom-tui/src/overlays.rs#L365)).

### Data shape (in [`crates/atom-tui/src/overlays.rs`](crates/atom-tui/src/overlays.rs))

```rust
pub struct ForkRow {
    pub kind: ForkRowKind,
    pub label: String,
    pub timestamp: Option<DateTime<Utc>>,
    pub message_id: Option<String>,  // None for the SessionLatest row
}

pub enum ForkRowKind {
    Header,        // ── Session ── / ── User messages ──
    SessionLatest,
    UserMessage,
}

pub fn filter_fork_rows<'a>(rows: &'a [ForkRow], q: &str) -> Vec<&'a ForkRow> { /* case-insensitive */ }
```

### App state ([`crates/atom-tui/src/app.rs`](crates/atom-tui/src/app.rs))

- `overlay_fork_rows: Vec<ForkRow>` near
  [`app.rs:158`](crates/atom-tui/src/app.rs#L158) (next to `overlay_sessions`).

### Slash dispatch ([`app.rs:1454`](crates/atom-tui/src/app.rs#L1454))

Add arm after `"/sessions" => {`:

```rust
"/fork" => {
    if self.streaming || self.remote_working {
        self.err_msg = "wait for the current turn to finish before forking".into();
        return Vec::new();
    }
    if self.read_only_view() {
        self.err_msg = "cannot fork a subagent session".into();
        return Vec::new();
    }
    self.overlay = Some(OverlayKind::Fork);
    self.overlay_q.clear();
    self.overlay_sel = 0;
    self.overlay_scroll = 0;
    self.working_msg = "loading session...".into();
    vec![Effect::LoadForkSource { id: self.session.id.clone() }]
}
```

### Effects & AppMsgs ([`crates/atom-tui/src/events.rs`](crates/atom-tui/src/events.rs))

```rust
// Effect
LoadForkSource { id: String },
ForkSession { source_id: String, message_id: Option<String> },

// AppMsg
ForkSourceLoaded { id: String, sess: Session, rows: Vec<ForkRow> },
ForkedSession { info: SessionInfo, draft: String },
```

### Effect handlers ([`crates/atom-tui/src/lib.rs`](crates/atom-tui/src/lib.rs))

`LoadForkSource`: spawn a task that calls `api::get_session(id)`, filters
`role == "user"` messages into `ForkRow::UserMessage`, prepends a
`ForkRow::SessionLatest` row, sends `ForkSourceLoaded`.

`ForkSession`: spawn a task that calls `api::fork_session(source_id, message_id)`,
sends `ForkedSession`.

### AppMsg handlers ([`app.rs:1648`](crates/atom-tui/src/app.rs#L1648))

- `ForkSourceLoaded`: install rows, reset `overlay_sel = 0` (the SessionLatest
  row), clear `working_msg`.
- `ForkedSession`: new method `forked_session(info, draft)` that mirrors
  `created_session()`
  ([`app.rs:1945`](crates/atom-tui/src/app.rs#L1945)) and additionally does
  `self.input.set_value(&draft)`. Subscribe to the new session.

### Confirm ([`app.rs:3171 confirm_overlay`](crates/atom-tui/src/app.rs#L3171))

Add a `Fork` arm: pick the selected `ForkRow`; emit `Effect::ForkSession { source_id, message_id }`. SessionLatest row → `message_id: None`; UserMessage row → `message_id: Some(message_id.clone())`.

### Renderer ([`crates/atom-tui/src/view.rs`](crates/atom-tui/src/view.rs))

Add `OverlayKind::Fork => render_fork_overlay(app)` to `render_overlay`
([`view.rs:1023`](crates/atom-tui/src/view.rs#L1023)). `render_fork_overlay`
reuses `overlay_query_lines` and `wrap_plain`:

- Header line "Session" + a select-able row showing the session title and
  `model · N msg` (no message id; this row is the SessionLatest variant).
- Header line "User messages".
- Filtered `ForkRow::UserMessage` rows, each rendered as
  `HH:MM  <truncated preview>`. Truncation respects the picker width; preview
  is a single visual line by default (two if the message fits cleanly).
- Footer: `<sel>/<total> user messages`.

### Key bindings

Reuse the existing overlay key handlers. `KeyCode::Up`/`Down`
([`app.rs:2934`](crates/atom-tui/src/app.rs#L2934),
[`app.rs:2953`](crates/atom-tui/src/app.rs#L2953)) get a `Fork` arm that
calls `overlays::move_fork_sel(self, dir)` to skip `ForkRowKind::Header` rows,
mirroring `move_session_sel`
([`overlays.rs:794`](crates/atom-tui/src/overlays.rs#L794)). Mouse hit-testing
needs `fork_row_at_y(app, y) -> Option<usize>` wired into the existing click
dispatcher.

## Edge cases

- **0 user messages**: render only the SessionLatest row, footer reads
  `no user messages yet`.
- **Compacted session**: post-compaction user messages only. Don't try to
  reconstruct pre-compaction text.
- **/fork while streaming**: refused with `err_msg`. Esc first.
- **/fork from a subagent session**: refused via `read_only_view()` (same gate
  as `/compact`,
  [`app.rs:1429`](crates/atom-tui/src/app.rs#L1429)).
- **/fork with no user messages selected and no SessionLatest visible**:
  unreachable; renderer always shows SessionLatest.
- **Two forks of the same session share a title** (`X (fork #1)`). Known wart;
  sibling-aware increment deferred.

## Tests

**Server** ([`crates/atom-server/tests/integration.rs`](crates/atom-server/tests/integration.rs)):
- `fork_from_latest_copies_full_transcript_and_returns_empty_draft`
- `fork_from_message_excludes_message_and_prefills_draft`
- `fork_rejects_non_user_message_id`
- `fork_appends_fork_1_suffix`
- `fork_returns_404_for_unknown_source`

**Store** ([`crates/atom-core/src/session/store.rs`](crates/atom-core/src/session/store.rs) test module):
- `fork_preserves_model_provider_cwd_thinking`
- `fork_child_has_parent_id_set`

**TUI** ([`crates/atom-tui/src/overlays.rs`](crates/atom-tui/src/overlays.rs)
and [`crates/atom-tui/src/app.rs`](crates/atom-tui/src/app.rs)):
- `fork_overlay_filter_drops_user_messages_keeps_session_latest`
- `fork_overlay_skip_header_rows_in_nav`
- `fork_overlay_enter_emits_fork_session_with_selected_message_id`
- `fork_overlay_rejects_streaming_session_with_err_msg`
- `forked_session_app_sets_input_draft`

**Renderer** ([`crates/atom-tui/src/view.rs`](crates/atom-tui/src/view.rs)):
- `fork_overlay_wraps_long_message_previews`

## Phasing

1. **PR 1 — data + server.** `Message.created_at`, `/fork` endpoint, server
   tests. No UI.
2. **PR 2 — TUI scaffolding.** `OverlayKind::Fork`, slash command, renderer,
   keys, search, hit testing. Enter is a stub that prints `working_msg`.
3. **PR 3 — wiring.** `LoadForkSource` / `ForkSession` effects, the
   `forked_session()` App method, draft pre-fill, end-to-end test.

Each PR keeps `cargo fmt --check`, `cargo clippy --workspace`, and
`cargo test -p atom -p atom-server -p atom-tui` green independently.

## Risks

- **Title duplication**: two forks of the same session share `(fork #1)`.
  Sibling-aware increment deferred — file a separate issue if it matters.
- **Exclusive boundary** is a UX choice. The plan and the desc default to it
  to match OpenCode; document the choice in a CHANGELOG entry when shipped.
- **No file isolation** — fork shares the working copy with the parent,
  matching OpenCode. A future per-fork worktree mode is a separate design.
