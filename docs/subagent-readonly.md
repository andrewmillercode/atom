# Subagents: user input disabled

Decision (2026-08): sessions with `parent_id` set (dispatch children) accept
no user input. The parent's `dispatch` tool is the only component that
starts or continues their turns. One exception: Esc stops the running child
turn immediately and records a user-initiated stop — never an error. The TUI
hides the prompt entirely in subagent views.

## Rationale

- The dispatch lifecycle assumes the parent drives the child. `cont` posts
  follow-ups, `cancel` kills turns, and `store::set_cancelled` assumes
  follow-ups arrive via the parent's dispatch. User-initiated child turns
  bypass this and race the parent-managed paths.
- Ownership is enforced elsewhere (`is_descendant_of` checks in
  `cont`/`cancel`). Direct input is the remaining gap.
- The user retains the emergency stop: the same Esc that pauses a primary
  agent's turn pauses a child's.

## Allowed

- Viewing the child transcript (Shift+Up).
- Esc to stop the running child turn (immediate, cooperative pause; partial
  output preserved).
- Answering sandbox approvals (`from_subagent` routing via the parent
  panel).
- Scrolling, selection, `/context`, and other non-turn operations.
- Parent-side dispatch `spawn`/`cont`/`cancel`.

## Server changes

Primary enforcement layer; the TUI gate alone leaves the HTTP API open.
`dispatch.rs` drives children in-process via `state.turns
.prepare_session_turn` (`dispatch.rs:485`, `:576`) and never calls its own
`/send` or `/pause` handlers, so HTTP-route classification is reliable:
requests on these routes are user-initiated.

1. `handle_send` (`http.rs:609`): after the session loads, if
   `!sess.parent_id.is_empty()`, return 409
   `"subagent sessions are managed by their parent"`.
2. `handle_pause` (`http.rs:436`): allowed for child sessions — this is the
   user's stop. Mark the pause user-initiated (e.g. a flag set in
   `AppState`, set here and consumed by the turn loop). No error message is
   recorded; `finish_paused_turn` already keeps the partial assistant reply.
3. `handle_compact`: 409 for child sessions, as before.

## Core changes (crates/atom-core/src/session/store.rs)

1. `DelegateStatus`: add `Stopped` (serialized `"stopped"`). `Cancelled`
   stays reserved for parent-kill and its revive logic
   (`dispatch.rs:257`).
2. Turn end (`turn.rs end_of_turn`): when the user-stop flag is set, record
   status `Stopped` instead of deriving `Done`/`Error` from the last
   assistant/error message.
3. Non-error transcript marker: append `Message { role: "stopped", content:
   "stopped by the user" }` to the child session when a user-initiated pause
   ends the turn. `child_result` (`dispatch.rs:31`) extends its tail scan to
   this role so `get_result`/`cont` report
   `"stopped by the user"` to the parent agent — without the `error:` prefix
   — and the marker in history tells the child model why it stopped on the
   next `cont`.
4. `end_of_turn` already broadcasts `{"type": "children"}` to the parent on
   child turn end; the new status propagates to the parent's TUI through
   that path.

## TUI changes (crates/atom-tui)

Condition throughout: `self.session.parent_id` is non-empty.

1. Prompt (`view.rs` draw): skip input rows, preview rows, and card padding.
   No replacement content. `Layout::compute` and `app.input_height()` reserve
   nothing, so `viewport_h` claims the rows; status and cwd footer rows stay
   consistent. Update hit-testing that assumes a prompt region
   (`content_pos_at`, image-chip clicks at `prompt_top_y`).
2. Cursor: not rendered.
3. Keys (`app.rs key()`): early-return for Char, Enter, Backspace, and `!`
   before the input path. Esc still clears selection; the
   `streaming -> PauseTurn` branch is kept so Esc stops the running child
   turn like a primary agent's. Remaining global bindings (Ctrl+P, Ctrl+C,
   Shift+Up) work unchanged.
4. Shell mode: disabled in subagent views. No `!` entry, no `RunShell`, no
   `PatchSessionCwd` against the child session.
5. Slash commands: client-side refusal of `/compact` in subagent views.
   View-only commands (e.g. `/context`) remain available.
6. Pending images: drafts are parent-session state; preserved across
   Shift+Up/Shift+Down, not editable in subagent views.

## Tests

Server:

- Child session: `POST /api/sessions/{id}/send` returns 409; same for
  `/compact`.
- Child session: `POST /pause` stops the turn, appends the `"stopped"`
  marker (non-error role), and records `DelegateStatus::Stopped`. No
  `"error"` message in the transcript.
- Dispatch `cont` still functions; `get_result` after a user stop reports
  status `stopped` and a tail without the `error:` prefix.

TUI:

- Subagent view: no prompt rows, `viewport_h` includes reclaimed rows, no
  cursor.
- Char and Enter produce no `SendTurn`; `!` produces no `RunShell` or
  `PatchSessionCwd`.
- Esc during a running child turn sends `PauseTurn`; Esc otherwise clears
  selection.
- Approvals answer; Shift+Up returns to the parent with prompt intact.

## Scope

All sessions with `parent_id` set. Scratch sessions are unaffected; a new
session or the parent is used to reach a model.
