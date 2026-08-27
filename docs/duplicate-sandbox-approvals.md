# Duplicate sandbox approval blocks

## Symptom

The same bash command renders as two identical `Sandbox` blocks, both resolvable
with "allowed this session". The second `RespondApproval` 404s server-side
(`http.rs:450-453`) and surfaces a transient `Errored` toast.

## Cause

`turn::emit` (`crates/atom-server/src/turn.rs:77`) fans every event out on two
transports: it writes to the turn's `/send` response **and** broadcasts to the
session's `/events` subscribers. The TUI holds both streams open for the same
session during a turn, so the parent's own `approval_request` arrives twice.
`/send` → `SendEvent` → `handle_stream_event`. `/events` → `sub_event`, which
bypasses the `live` guard for `approval_request` (`crates/atom-tui/src/app.rs:2024`).
The handler (`app.rs:933`) pushes a block with no dedup → two blocks, same id.

The bypass exists for a reason: subagent approvals only reach the parent via the
explicit `subs.broadcast(parent_id, &ev)` in `ServerApprover::decide`
(`dispatch.rs:240`) — the child's own emit goes to `EventOut::Discard`. Dropping
them while streaming would hang the blocked child.

The bug is that the exception matches *every* approval, not just subagent ones.
Your own approval already arrived on `/send` and was rendered; the exception
then lets the identical `/events` copy through as well. Nothing ever asks "have
I already rendered this approval id?"

## Secondary path

Reconnect replay in `handle_events` (`crates/atom-server/src/http.rs:528-556`)
re-sends pending approvals to a resubscribed viewer. With `/send` still live the
same bypass lets the replayed copy through as well.

## Fix

`crates/atom-tui/src/app.rs`, `sub_event`:

```rust
if ev.event_type == "approval_request" {
    let is_own = !ev.from_subagent
        && (ev.session_id.is_empty() || ev.session_id == self.session.id);
    if self.streaming && is_own {
        return Vec::new();
    }
    let effects = self.handle_stream_event(&ev);
    self.refresh_viewport();
    return effects;
}
```

Plus id-dedup in the `approval_request` arm of `handle_stream_event`: skip if
`self.approval.as_ref().is_some_and(|a| a.id == ev.id)` or a block already
carries an `InlineApproval` with that id — covers the replay path.

## Why /send and /events both exist

Two scopes, one fan-out:

- **`/send`** (POST `/api/sessions/{id}/send`) is *turn-scoped*. It exists only
  while one turn runs and dies with the HTTP request. It carries the
  submitting client's own turn events (round_start, content, reasoning, tool,
  tool_result, done). Authoritative while your turn paints.
- **`/events`** (GET `/api/sessions/{id}/events`) is *session-scoped*. Opened on
  session load, persists across turns. Carries everything else: other clients'
  activity (`user_message`, `saved`, `paused`, `title`), parent-panel updates
  (`children`, `dispatch_result`), subagent approval broadcasts, and reconnect
  replay of pending approvals.

Server-side both are fed by the same `emit()`. A client that is both submitting
and watching therefore receives each event twice by design — the TUI drops the
`/events` copy while `streaming` is true (the guard at `app.rs:2055`, "avoid
duplicate tool blocks and overlapping text"). The approval carve-out predates an
ownership check, which is the bug above.
