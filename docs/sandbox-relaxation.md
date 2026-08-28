# Sandbox Relaxation Design

Note: at the time of writing, sandbox approval from user still gets blocked by seatbelt. Sandbox v2 should fix this.


## Problem

The current sandbox is conservative:
- Unknown commands → Ask (blocks the session)
- Known-dangerous commands → Deny (agent sees error, continues)
- Only a small allowlist of read-only tools auto-allow
- Network is denied by default (even `curl` to localhost fails)
- File writes outside workspace require explicit approval
- The 60s timeout auto-denies silently → agents lose context

Agents are frequently interrupted for routine operations like:
- Building/testing code (`cargo check`, `make`)
- Reading system config files (already allowed by `read_file`)
- Running linters, formatters, documentation generators
- `git` operations within the workspace
- Language-specific REPLs (`python -c "..."`) for quick computations

## Goals

1. **Fewer interruptions** for clearly safe operations
2. **No silent data loss** (the old timeout → deny is gone)
3. **Progressive trust**: sessions earn broader permissions through use
4. **Immediate escape hatch**: Esc/pause always cancels
5. **Auditable**: every decision logged regardless of mode

## Proposed Changes

### A. Trust Tiers (replaces flat Allow/Ask/Deny)

```
Tier 0 — Always Allow (no prompt, no confinement)
  • Pure reads: cat, head, tail, less, wc, file, stat, ls, tree, find (read-only)
  • Workspace builds: cargo {build,check,test,clippy,fmt}, make, npm run, go build
  • Git reads: git {status,log,diff,show,branch,stash list}
  • Formatters: prettier, black, rustfmt, gofmt
  • Package lockfile: cargo update (no --precise), npm install (with lockfile)

Tier 1 — Allow With Confinement (auto-allow, kernel sandbox applied)
  • Interpreters with workspace-only FS: python -c, node -e
  • Script execution within workspace: ./scripts/*, *.sh
  • Network reads to localhost/127.0.0.1 (test servers)

Tier 2 — Session Trust (auto-allow after first approval in session)
  • Git write ops: git push, git commit
  • Network tools: curl, wget, httpie (first use prompts, rest auto)
  • File writes outside workspace (after first approval per directory)

Tier 3 — Always Ask (every invocation prompts)
  • Package installs: pip install, npm install (without lockfile)
  • System package managers: apt, brew
  • Docker/container ops
  • SSH, scp, rsync to remote

Tier 4 — Deny (hard block, no prompt)
  • rm -rf / (catastrophic patterns)
  • dd, mkfs, mount (system-level destruction)
  • chmod 777, chown (permission escalation)
  • eval/exec with piped remote content
```

### B. Confinement Profiles (graduated)

Instead of one-size-fits-all Seatbelt profile:

| Profile | FS Write | FS Read | Network | Process |
|---------|----------|---------|---------|---------|
| `read-only` | workspace temp only | anywhere | deny | deny fork |
| `workspace` (current) | workspace + /tmp | anywhere | deny | allow |
| `workspace+net` | workspace + /tmp | anywhere | allow | allow |
| `open` | anywhere | anywhere | allow | allow |

Auto-select profile based on command analysis:
- Tier 0 commands → no confinement (already safe by definition)
- Tier 1 commands → `workspace` profile
- Tier 2+ approved commands → `workspace+net` if they need network

### C. Directory Trust (for file tools)

Replace the binary "in workspace / not in workspace" check with a
directory trust model:

```json
{
  "trusted_write_dirs": [
    "~/projects/*",
    "~/.config/*"
  ],
  "trusted_read_dirs": [
    "/",  // read anywhere is fine
  ]
}
```

**First write** to a new directory tree → prompt. Once approved for the
session (or globally), all subsequent writes to that tree pass through.

This solves the kitty.conf case: once you approve `~/.config/kitty/`,
all future config edits in that tree auto-allow.

### D. Smart Auto-Approval Heuristics

Beyond static rules, use runtime context:

1. **Intent matching**: If the model's tool call is `write_file` to a path
   the model previously `read_file` from (same session), auto-allow —
   the model is editing something it was explicitly shown.

2. **Workspace ancestry**: If the target path is a sibling/child of the
   workspace (e.g., `../other-project/`), treat as semi-trusted
   (session-approve on first use).

3. **Command continuation**: If the model ran `git clone X` (approved), then
   immediately does `cd X && cargo build`, the build should auto-allow
   since it's in a directory the model just created.

4. **Tool-chain awareness**: After approving `npm install`, auto-allow
   `npx`, `node_modules/.bin/*` invocations since they're part of the
   same workflow.

### E. "Sticky Session" Mode

A new sandbox mode for power users:

```json
{ "mode": "sticky" }
```

Behavior:
- First invocation of any Ask-tier command: prompt as usual
- If approved with `[s]` or `[g]`: all future invocations of commands
  matching the same rule ID auto-allow for the session
- Effectively: after 2-3 approvals at session start, the session runs
  uninterrupted
- Still confined by the kernel sandbox (crash safety)
- Still denies Tier 4 catastrophic commands

### F. Background Batch Mode

For long-running agent tasks (subagents, overnight jobs):

```json
{ "mode": "batch" }
```

Behavior:
- Tier 0-1: always allow
- Tier 2: auto-allow if previously approved globally
- Tier 3: queue the approval, don't block — run other tools first,
  come back to blocked ones when user is available
- Tier 4: deny

This prevents a single missing approval from blocking an entire
multi-hour agent run.

### G. Implementation Priority

1. **Remove timeout** (done ✓) — approvals block indefinitely
2. **Inline rendering** (done ✓) — approval visible as tool block
3. **Sticky session mode** — immediate value, low complexity
4. **Directory trust for file tools** — solves out-of-workspace UX
5. **Expanded Tier 0** — fewer prompts for obviously safe ops
6. **Background batch mode** — for unattended subagents

## Migration

- Default mode stays `workspace` (unchanged behavior minus timeout)
- New modes opt-in via `/sandbox` slash command or `sandbox.json`
- Existing global approvals carry forward unchanged
- Session-scoped approvals are already in-memory only
