# Sandbox v2

Rewrite of the bash-command gating system. Replaces the v1
allow/ask/deny rule ladder + Seatbelt confinement with a three-tier
escalation model: safe commands run silently, unknown commands get a
cheap LLM review, and only commands that fail review (or hit a
guardrail) interrupt the user.

Status: spec. Not implemented.

## Problem

The v1 sandbox is conservative and interrupts constantly:

- Unknown commands → prompt (blocks the session)
- Network denied by default — even `curl` to a test server needs approval
- All builds/installs (`cargo build`, `npm install`) prompt
- The approval flow is fragile: a kernel-sandbox (Seatbelt) bug can
  block a command the user already approved
- File-boundary violations and path escapes each have their own prompt
  path, producing a patchwork of rules the user can't reason about

The v1 answer to every gray area was "ask." v2's answer is "escalate up
the chain, and only land on the user as a last resort."

## Design

Every bash command is classified into one of three tiers. Failures
escalate strictly upward: 1 → 2 → 3. Nothing escalates sideways or down.

```
Tier 1 — run            known-safe, additive, or user-approved commands
Tier 2 — auto-review    unknown commands go to a cheap LLM reviewer
Tier 3 — prompt         the user decides (accept/deny, once/global)
```

### Tier 1 — run, no prompt

Commands the static rule table recognizes as additive or read-only.
Includes, non-exclusively:

- Pure reads: `cat`, `head`, `tail`, `ls`, `stat`, `find`, `grep`, `wc`
- Git reads: `git status|log|diff|show|branch`
- Builds and checks: `cargo build|check|test|clippy|fmt`, `make`,
  `npm run`, `gofmt`, `prettier`, `black`
- Package **installs**: `cargo update|add`, `npm install`, `pip install`
- **Network**: `curl`, `wget`, `ping` — unconfined, any destination

The tier intentionally includes network and installs. This is a posture
decision, not an oversight: footgunning the agent into a review prompt
every time it researches or fetches a dependency costs more than the
occasional bad package, which is reviewable after the fact and cannot
self-propagate (no confinement, no second tier it can hide in).

A matching **user rule** in `sandbox.json` (`rules.allow`, written by a
previous "accept all") also lands here.

**Resolvability requirement**: a command is only Tier 1 if every token
is statically resolvable. Command substitution (`$(…)`), backticks,
process substitution (`<(...)`), variable references (`$FOO`), or
unexpanded glob-into-flag shapes mean the table cannot actually see
what will run — those commands are Tier 2 at best, never Tier 1. This
closes env-var indirection as a Tier 1 evasion for the guardrail shapes
above.

**Arg verification**: Tier 1 matches the *whole argv*, not just the
head. Read tools carry execution/write-capable flags that turn them
into command primitives: `find -exec/-execdir/-delete`, `file
-f/-m`, `sort -o` (writes), an unrecognized flag on any Tier 1 tool.
Shape-matched but flag-vetoed ⇒ at least Tier 2. A command whose
analysis cannot be completed (over-long, weird quoting, parse failure)
prompts — the analyzer must not give up to Tier 1 by default.

**Normalization before matching**: argv is NFKC-normalized with
zero-width characters stripped before any table or guardrail matching
(`rm -rf ∼` with U+223C ≠ `~/`); guardrail path shapes match
**canonicalized absolute paths**, so `/Users/./me`, `..` chains, and
symlink indirection resolve before comparison.

**Generality**: the table intentionally covers only the universal core
(reads, git reads, cwd ops, the few cross-language tools). It does not
try to enumerate every language or domain — `mix test`, `dotnet
publish`, `sips`, `ffmpeg` have no entries and fall to Tier 2, where
the reviewer generalizes without per-ecosystem rules. Coverage grows
organically: reviewer allow-verdicts persist as learned rules, and user
"accept all" decisions persist as allow rules. atom is usable in any
language (or no language) from first run; Tier 1 just gets sharper the
longer it runs on your machine.

### Tier 2 — auto-review

Anything the rule table does not recognize. The command is sent to a
small, cheap LLM reviewer that decides: run it, or escalate.

The reviewer is a **reviewer swap, not a permission system** — it exists
to classify the unknown, not to police Tier 1 (see Guardrails for its
limits). Cursor's Auto-review and Codex's auto-review are the prior art
to steal from; both use exactly this shape.

- **Model**: smallest model with enough reasoning to judge a shell
  command. Undecided — research Cursor Auto-review (Haiku-class managed
  model) and Codex auto-review before picking. Runs on the existing
  provider infra from `atoms`.
- **Input (v1, single-shot)**: full command line, cwd, workspace root,
  and the user's current request. Deliberately **not** pasted tool
  outputs, web pages, or file contents — the reviewer's context is a
  prompt-injection surface, and the cheapest hardening is to keep
  attacker-controlled text out of it.
- **Steering**: plain-English `allow_instructions` /
  `block_instructions` from config, Cursor-`permissions.json` style.
- **Verdict**: allow → run. Fishy → escalate to Tier 3. The escalation
  goes straight to the user prompt (no agent-feedback retry loop in v1).
- **Learning**: an allow verdict persists a prefix rule (`cargo test
  *`) to `rules.learned` in `sandbox.json` — prior yes ⇒ yes forever.
  Constraints, because a poisoned learned rule is silent forever:
  prefixes are capped at head + one subcommand or flag group
  (`cargo test *`, never `cargo *`); commands containing unresolvable
  constructs or protected-path targets are never learned; guardrail
  families are never learned; and a small **never-learn veto list**
  excludes verbs whose blast radius outruns any prefix — `git push`,
  `git remote`, `scp`, `rsync`, `ssh`, cloud/deploy CLIs. Those always
  re-review or prompt. Learned rules are human-editable/prunable
  and never override guardrails, user deny rules, or protected paths.

### Tier 3 — prompt

The existing approval flow, unchanged mechanically: the client shows the
command inline as a tool block and blocks indefinitely (no timeout —
already fixed in v1.5). New decision set:

```
[y] accept once   [a] accept all
[n] deny once     [d] deny all
```

**Trust chrome.** Prompt text is invariant scaffolding rendered by the
client: the full command, the cwd, and the decision origin (guardrail /
Tier 2 review / rule name). Tool output is rendered with terminal
escape sequences neutralized, so session output can never draw a fake
approval block, and key presses always belong to the real chrome —
an injected session cannot phish you into approving B by drawing A.

- **Accept once** — runs; nothing remembered.
- **Accept all** — runs, and writes a **prefix rule** to
  `sandbox.json` so the command family lands in Tier 1 forever
  (e.g. approving `cargo test --release` stores `cargo test *`).
  Prefixes generalize at word boundaries only; the prefix is capped so
  dangerous heads never generalize silently (see Guardrails).
- **Deny once** — error to the model, it continues with another approach.
- **Deny all** — error now, and a persistent deny rule: this command
  family prompts **never** again; the model is told it is blocked.
- No session-scoped grants anywhere. Once/all only.

Replaces the current `[a]/[s]/[g]/[d]` buttons and the `allow_session`
decision.

### Guardrails (static escalation floor)

A hardcoded pattern list that **overrides every other decision except
the user's**: a match is forced up to Tier 3, no matter what the
reviewer says. This is the reviewer's guardrail — it saves the reviewer
from having to reason about catastrophic shapes, saves tokens, and
means the reviewer can never talk a destructive command down to Tier 1
or 2.

Guardrail match ⇒ Tier 3 prompt (not a hard deny — the user still
decides, and can deny-all it permanently).

Initial list:

- Recursive/forced deletes targeting `$HOME`, system roots, or outside
  the workspace (`rm -rf /...`, `rm -rf ~`)
- Disk-level tools: `dd`, `mkfs`, `diskutil erase*`, `mount`
- Privilege/permission escalation: `sudo`, `chmod`, `chown`
- Process-kill tools: `kill`, `killall`, `pkill` — any variant, any
  target. A wrong process name can take down the session or system
  daemons, so there is no safe subset.
- Credential exfil shapes: file-bearing network payloads — `curl|nc -d @…`,
  `-T`, `--data-binary`, `<` redirections — referencing `$HOME`,
  protected paths, or dotfiles, and piping `~/.ssh/*`, `.env`,
  keychains, or token-shaped args into network tools.
- Keychain access: `security find-*`, `security dump-keychain` —
  one command exports every stored secret.
- Network-to-interpreter pipes: `curl … | sh|bash|zsh|python`,
  `wget -O - … | sh` — remote-download-then-execute in one line.
- System services and automation: `osascript`, `launchctl`,
  `crontab`, `defaults write` outside the workspace.

The reviewer may still escalate *additional* commands to Tier 3 on
its own judgment — guardrails only set a floor.

### Protected paths (write floor, all tools)

A small set of paths no tier may write and **no tool** — bash or file
tools — may modify without a prompt. This is the anti-self-escalation
floor: an agent that could edit its own gate owns the gate.

- atom's own state: `dataDir()` (`sandbox.json`, approval/learned-rule
  stores) and the atom config dir
- Shell startup files: `~/.zshrc`, `~/.zshenv`, `~/.zprofile`,
  `~/.bashrc`, `~/.bash_profile`, fish config — a tier-1 `echo >>` or
  file-tool edit here persists attacker code into every future shell
- Anything on `$PATH`: `~/.local/bin`, `/usr/local/bin`,
  `/opt/homebrew/bin` — a tier-1 `cp` of a shim over a trusted binary
  turns later tier-1 runs into attacker code
- `~/.ssh` (writes; reads remain per the open posture)
- `.git/hooks/` in any repository the agent touches — a planted hook
  turns every later `git commit` into host-level code execution

Protected-path matching also checks **symlink targets**: a write whose
destination or any path component resolves into a protected tree is
protected, wherever the link lives.

Protected-path writes are **Tier 3, always** — they cannot be
pre-allowed by `rules.allow` or `learned` entries, only accepted
per-operation (or silenced via deny-all).

### Environment scrubbing

Bash subprocesses run with a **scrubbed environment**: provider
credentials (`ANTHROPIC_*`, `OPENAI_*`, `GITHUB_TOKEN`, and
token-shaped variables such as `*_TOKEN`, `*_KEY`, `*_SECRET`, `*_PASSWORD`)
are unset before every command runs. The shell inheritance path —
`printenv`, `env`, `$ENV_VAR` indirection — is otherwise the cheapest
credential-exfiltration channel in the whole design, and it needs no
static evasion at all. Scrubbing is deterministic, zero-prompt, and
composes with the exfil guardrails instead of relying on them.

### Compound commands

Segment on top-level `&&`, `||`, `;`, `|` (existing tokenizer). Each
segment is classified independently; the command runs only if every
segment clears, and the highest tier among segments wins. A Tier 1 +
Tier 3 mixture prompts once for the whole command line.

**Wrapper unwrapping**: interpreters and exec wrappers — `bash`,
`sh`, `zsh -c`, `env`, `nice`, `timeout`, `nohup`, bare `xargs` — are
unwrapped recursively before classification: the real command (for
`-c`-style shells, the parsed string) is what gets tiered. `bash
killall Dock` is a `killall`, never a Tier 1 "shell". A wrapper the
unwrapper cannot see through is Tier 2 by default. Without this,
`bash <anything>` is an unclimbable blind spot in the table and an
ambiguous input to the reviewer.

## What is removed

- **Seatbelt / kernel confinement entirely.** No `sandbox-exec`
  profiles, no confinement matrix, no `network` policy knob. Tiers +
  review + user decisions are the whole system. This also kills the
  "approved command still blocked by seatbelt" defect class. If a
  kernel backstop is ever wanted again, it can be reintroduced around
  Tier 2 only (the softest decision in the chain) — noted, not designed.
- **Session-scoped approvals** (`allow_session`, `ApprovalStore`
  sessions, `[s]` button).
- **Trust-tier permission matrix, confinement profiles, sticky mode,
  batch mode, audit log, directory-trust config** — all v1 spec ideas
  that the escalation model supersedes.
- **Gate on file tools — except protected paths.**
  `read_file`/`write_file`/`edit_file` stay ungated for ordinary files;
  v2 guards bash. But writes to **protected paths** (atom's own
  `sandbox.json`, shell rc files, `$PATH` dirs, `~/.ssh`) go through
  the gate regardless of which tool performs them — otherwise the agent
  edits its own config with `edit_file` and self-escalates. Bash-side
  writes outside the workspace are handled by classification: additive
  writes (`mkdir -p`, `cp` to a new path) are Tier 1; destructive
  shapes (`rm`, `mv` over existing targets) are reviewer/prompt
  territory.
- **`sandbox.json` v1 fields**: `mode` (off/workspace/strict), `network`,
  `extra_writable`, `extra_readonly`. One behavior for everyone.

## Configuration

`sandbox.json` v2 schema:

```json
{
  "version": 2,
  "auto_review": {
    "enabled": true,
    "model": null,
    "allow_instructions": [],
    "block_instructions": []
  },
  "rules": {
    "allow": ["cargo test *", "git push *"],
    "learned": ["mix test *"],
    "deny":   ["rm * ~"]
  }
}
```

- `model`: reviewer override; `null` = built-in default (TBD).
- `allow` / `deny` rules: command-prefix globs, human-editable. `allow`
  → Tier 1, `deny` → Tier 3 prompt with the rule name given as reason.
- `learned`: prefix rules written automatically by Tier 2 reviewer
  allow-verdicts. Treated as `allow` with lower provenance: guardrails
  and `deny` rules always win over `learned` entries.
- `allow_instructions` / `block_instructions`: plain-English steering
  for the reviewer.
- Guardrails are hardcoded, not config: they are the floor the config
  cannot lower.
- **Migration**: one-time load of v1 `sandbox.json` + `approvals.json`;
  existing global grants become `allow` prefix rules; v1 fields are
  dropped. Missing file = defaults.
- **Concurrency**: many atom clients/servers run against one
  `sandbox.json`. All rule writes go through a single-writer lock and
  atomic temp-file + rename, and writers re-read before append so
  concurrent sessions never drop each other's rules.

## Implementation map

- `atom-sandbox`: delete `seatbelt.rs`; drop confinement from `exec.rs`;
  `rules.rs` verdicts become tier classification (Allow→1, Ask→2,
  fallback→2, guardrail match→3); `approvals.rs` loses session grants.
- New: reviewer module (`atom-sandbox` or `atom-server`) — provider call,
  steering instructions, timeout → escalate.
- `atom-tui`: `approval_buttons()` → the new four-button set; verdict
  display notes which tier/rule fired.
- `atom-server/http.rs`: decision decoding loses `allow_session`.

## Open questions (pre-implementation)

1. Reviewer model choice; latency budget (should be ≲ 2s end to end).
2. Prefix-rule generalization rules for "accept all" — exact
   algorithm for where the wildcard lands (two-word default, flag-token
   exceptions, cap for dangerous heads like `rm`).
3. Whether the reviewer verdict is displayed inline (a small
   `reviewed: ok` line in the tool block) or only on escalation.

## Residual risks (accepted, by posture decision)

v2 removes the kernel backstop and keeps network + installs unconfined.
These follow from that and are documented, not unknown:

1. **Creative exfiltration via Tier 1.** Reads are Tier 1, network is
   Tier 1, and static shape matching cannot enumerate encodings. An
   injected agent that reads a secret and sends it out through a shape
   the exfil guardrail doesn't list (base64 indirection, heredoc into
   an unlisted tool, a Tier 1 interpreter the table trusts) can
   exfiltrate data. The two big cheap channels are closed anyway —
   subprocess environments are scrubbed and keychain access is
   guardrailed — so what's left is file content the user can already
   read, and the guardrail list is expected to grow with real cases.
2. **Install-script code execution.** `npm install`/postinstall runs
   arbitrary code at Tier 1, and without confinement it can do
   anything that code wants on first run. Accepted: malicious packages
   are post-hoc reviewable and cannot pre-stage broad persistence
   (rc/PATH/atom-config/.git-hook writes are protected paths;
   kill/automation/keychain tooling is guardrailed). Same posture
   covers executing downloaded scripts: never Tier 1, reviewer cannot
   read the script's contents, so expect escalation to a prompt — if
   the reviewer must ever inspect a script, that is a deliberate
   extension with its own injection analysis.
3. **Reviewer misclassification.** The reviewer is an LLM; both false
   allows and false escalations happen, and its input could still carry
   injection through the command string itself. It is best-effort
   convenience over the hard floors, never the boundary.
4. **Denial-of-work via guardrails.** The floor prompts on broad
   classes (`kill`, `osascript`); a task legitimately needing them
   costs one prompt and prefix-rule each in early sessions. Friction
   is the price of the floor, not a defect — the relief valves are
   prefix rules (accept all) and pruning deny rules.