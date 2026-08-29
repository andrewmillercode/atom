# Sandbox v2

Rewrite of bash-command gating. Two stages: a wide static Allow list
runs commands silently, and anything not on the list prompts the user
with a clear, terminal-native block. A hardcoded floor of dangerous
shapes (rm-rf /, dd to devices, sudo, keychain, curl|sh, …) blocks
even approved commands. No LLM step.

Status: spec. Not implemented.

## Problem

v1 is conservative and interrupts constantly. Network tools and
installs always ask, even `cargo build` and `npm install`; the Seatbelt
confinement can block a command the user already approved; the
prompt is fine but tiny compared to the noise it produces. v2's answer
is "be generous about what runs, make the prompt clean when it fires,
and keep a hard floor that no path can lower."

## Design

```
Static table — Allow  verdict   → run
Guardrail     — Deny   verdict   → blocked (terminal)
Static table  — Ask    verdict   → user prompt
User prompt   — once / all / deny once / deny all
```

No Tier 2 LLM review. The classifier is the rule table plus arg/path
analysis; the only humans involved are the agent and the user. That
removes a costly round trip and the largest prompt-injection surface
in the old design (reviewer input would have been attacker-controlled
text), at the cost of more prompts for commands the table doesn't
recognize. The wide table below is what makes the tradeoff pay off.

## Tier 1 — wide static allowlist

A command is Tier 1 only when every token is statically resolvable
(no `$(…)`, backticks, `<(…)`, `$FOO`, or unexpanded glob-into-flag),
argv is NFKC-normalized with zero-width chars stripped, and every
argument that looks like a write target is canonicalized before
matching (so `/Users/./me` and `..` chains resolve). Flag-vetoed
shapes — `find -delete/-exec`, `file -f`, `sort -o`, `sed -i`,
`tar -x`, etc. — drop to the prompt tier even when their bare form
is Tier 1.

Categories, with the safety argument for each:

**A. Pure reads** — no writes, no network, no side effects.
`cat head tail less more nl`, `ls tree eza exa lsd`, `find` (no
`-delete/-exec/-ok`), `stat file du df`, `wc sort uniq cut paste tr
column fold fmt nl tac rev join comm`, `diff cmp colordiff patch
--dry-run`, `grep egrep fgrep zgrep rg ag ack fd fdfind`, `awk sed`
(no `-i`), `jq yq xmllint`, `xxd od hexdump strings`, `md5 md5sum
shasum sha1sum sha256sum sha512sum cksum`, `base64 -d` (decode only),
`bc expr`, `which whereis type whence command hash`, `whoami id
groups hostname uname arch sw_vers date pwd locale true false test
sleep env printenv` (filtered in prompt — see Environment
scrubbing).

**B. Builds, tests, formatters** — write artifacts to cwd/build dirs.
`make ninja meson cmake --build`, `cargo build|check|test|clippy|fmt
|doc|run|bench|clean`, `go build|test|run|vet|mod tidy|mod
download`, `python -m pytest|unittest`, `pytest ruff mypy pyright
black isort`, `node tsc ts-node tsx deno test|check|cache`, `bun
test|run|build`, `npx pnpm run|test|build|exec` (not `pnpm
publish`), `yarn run|test|build`, `ruby bundle exec rake rspec`,
`swift build|test|run|package xcodebuild`, `mix compile|test|run
|docs elixir`, `dotnet build|test|run|restore`, `mvn gradle`, `gofmt
prettier shellcheck shfmt`.

**C. Package installs** — fetches and runs install scripts; the
postinstall risk is accepted (see Residual risks). `cargo add|update
|install`, `npm i|install|add|update|ci` (not `npm publish`),
`pnpm i|install|add|update`, `yarn install|add`, `bun install|add`,
`pip pip3 pipx`, `uv pip|add|sync|run`, `poetry install|add|update
|run`, `gem install`, `bundle install`, `go get|install`, `mix
archive.install`, `dotnet add package|tool install`, `brew
install|upgrade|reinstall|tap` (mac), `apt apt-get aptitude
install|update|upgrade` (Linux), `yum dnf zypper pacman apk add`,
`asdf install`, `nix profile install`. `cargo publish`, `npm
publish`, `dotnet nuget push` stay at the prompt tier.

**D. Network fetches** — bytes in, no execution. `curl wget http
httpie`, `gh api glab api`, `ping traceroute mtr`, `dig host
nslookup whois`, `ssh-keyscan`, `git fetch` (updates
remote-tracking branches only).

**E. Local VCS — additive / reversible** — no force pushes, no
hard resets, no clean of untracked. `git status|log|diff|show|blame
|branch|tag|remote|config --get|reflog|ls-files|shortlog|rev-parse
|describe|worktree|help|version`, `git add`, `git rm --cached`,
`git mv`, `git commit` (no `--amend`), `git checkout -b`, `git
switch -c`, `git fetch`, `git worktree add`, `git stash|apply|pop`,
`git tag`, `git init`, `git revert`, `git merge` (no `-ff-only`
overrides), `git rebase` (no force). `git push`, `git reset --hard`,
`git clean -fd`, `git branch -D` stay at the prompt tier.

**F. Filesystem additions** — within cwd only; writes outside cwd
drop to the prompt tier. `mkdir -p`, `touch`, `cp ln mv install
rsync truncate` (target must not preexist for `cp`/`install`), `tee`,
`tar -c zip -r gzip bzip2 xz zstd` (creation only), `git init`.

**G. Process / system read-only** — `ps top -l pgrep -l`, `lsof
netstat ss ifconfig ip`, `mount` (no args), `diskutil list|info|apfs
list`, `dtrace` read probes, `iostat vm_stat sysctl -n`, `uptime
launchctl list` (read-only list mode).

**H. Dev workflow helpers** — `docker ps|images|logs|inspect
|version|info`, `docker compose build|up|down|ps|logs|config`, `kubectl
get|describe|logs|version|config view`, `nix build|develop|run|flake
update`, `make -n` (dry-run).

Commands outside the table are Tier 2 — prompt the user. Same
shape, same chrome, same prefix-rule growth. The table is the
default; users widen it through accept-all decisions, never the
other way.

## Tier 2 — prompt

The user prompt is the only interruption Tier 1 cannot absorb.
Existing chrome stays, with refinements listed below.

Decisions:

```
[y] accept once    run, no memory
[a] accept all     run + persist prefix rule
[n] deny once      error back to the model
[d] deny all       error now + persist deny rule
[esc] cancel       error back, nothing persisted
```

Session-scoped grants are gone. Once/all only — the prompt either
remembers or doesn't, never in between. `[a]` writes a **prefix
rule** to `sandbox.json` so the command family lands in Tier 1
forever (e.g. approving `cargo test --release` stores `cargo test
*`); `[d]` writes a deny rule so the family prompts never again.

**Trust chrome.** Prompt text is invariant scaffolding rendered by
the client: full command, cwd, matched rule id, reason, and the
decision origin (guardrail / arg-veto / unknown-command / rule
name). Tool output in the prompt block has terminal escapes
neutralized, so session output can never draw a fake approval row,
and key presses always belong to the real chrome — an injected
session cannot phish you into approving B by drawing A.

**Subagent provenance.** When the prompt is from a dispatched
subagent, the parent view surfaces it without navigating into the
child (carries `from_subagent` + `child_title` on the event); the
child waits indefinitely; cancel propagates up.

**Editable prefix on accept-all.** `[a]` shows the resulting
prefix before persisting ("this would let all `cargo test`
invocations run unprompted"); the user can adjust or back out.
Dangerous heads (`rm`, `git push`, `sudo`, `chmod -R`,
`network-to-interpreter` shapes) are flagged but not blocked —
accepting still works, the warning is informational.

**Help line.** A single dim line under the buttons lists every key
binding, the prefix-rule preview, and a one-line "press ? for
details" hint that expands each decision into its long form in a
modal overlay.

**Audit.** Every prompt and decision is written to `sandbox-audit.log`
alongside the existing verdict record — `[a]` and `[d]` lines
include the rule added, so denials and grants can be reviewed.

## Guardrails (Deny floor)

A hardcoded pattern list that overrides every other decision: a
match is blocked outright, no prompt, even for a user who has
accepted the command before. The floor is what makes "wide Tier 1"
safe — the table can be generous because the floor catches the
shapes that matter.

- Recursive/forced deletes targeting `$HOME`, system roots, or
  outside the workspace: `rm -rf /`, `rm -rf ~`, `rm -rf $HOME`,
  fork-bomb `:(){ :|:& };:`
- Disk-level: `dd of=/dev/*`, `mkfs*`, `diskutil erase*|apfs|hfs`,
  `mount` with device
- Privilege escalation: `sudo`, `su`, `doas`, `pfexec`, `dzdo`,
  `csrutil`, `nvram`, `pmset`, `kextload/unload/util`, `spctl`,
  `installer`, `dscl`
- Process kill: `kill`, `killall`, `pkill` — any variant, any
  target. A wrong name takes down the session or system daemons;
  there is no safe subset.
- System services and automation: `osascript`, `launchctl`
  (mutating forms), `crontab`, `defaults write` outside the
  workspace, `shutdown`, `reboot`, `halt`, `poweroff`, `init`,
  `telinit`
- Credential exfil shapes: network tools with file-bearing
  payloads — `curl|nc -d @…`, `-T`, `--data-binary`, `<` redirections
  — referencing `$HOME`, protected paths, or dotfiles, and piping
  `~/.ssh/*`, `.env`, keychains, or token-shaped args into network
  tools
- Keychain: `security dump-keychain`, `security find-*` (any)
- Network-to-interpreter: `curl … | sh|bash|zsh|python`,
  `wget -O - … | sh`
- Path escape write: any write-prog (`touch mkdir cp mv rm ln tee
  truncate install rsync dd`) targeting `/System`, `/bin`,
  `/sbin`, `/usr`, `/etc`, `/private/etc`, `/boot`, or any
  symlink whose target resolves there

Guardrail matches are logged but never reduced by `[a]` or any
prefix rule — a deny rule written by `[d]` is the only way to
silence a guardrail, and only when the user does so deliberately.

## Protected paths (Tier 1 floor, all tools)

No tier may write these without a prompt. This is the
anti-self-escalation floor — an agent that could edit its own gate
owns the gate.

- atom's own state: `dataDir()` (`sandbox.json`, `approvals.json`,
  audit log) and the atom config dir
- Shell startup files: `~/.zshrc`, `~/.zshenv`, `~/.zprofile`,
  `~/.bashrc`, `~/.bash_profile`, fish config — a Tier 1
  `echo >>` or file-tool edit here persists attacker code into
  every future shell
- Anything on `$PATH`: `~/.local/bin`, `/usr/local/bin`,
  `/opt/homebrew/bin`, any other `PATH` entry — a Tier 1 `cp` of a
  shim over a trusted binary turns later Tier 1 runs into attacker
  code
- `~/.ssh` (writes; reads remain per the open posture)
- `.git/hooks/` in any repository the agent touches — a planted
  hook turns every later `git commit` into host-level code
  execution

Protected-path matching checks symlink targets too: a write whose
destination or any path component resolves into a protected tree
is protected, wherever the link lives. Bash-side writes to
protected paths escalate to the prompt tier regardless of the
static rule; the file tools enforce the same floor (writes to
`sandbox.json` from `edit_file` would otherwise bypass the gate).

## Environment scrubbing

Bash subprocesses run with a scrubbed environment: provider
credentials (`ANTHROPIC_*`, `OPENAI_*`, `GITHUB_TOKEN`, and
token-shaped `*_TOKEN`, `*_KEY`, `*_SECRET`, `*_PASSWORD`) are
unset before every command runs. `printenv` and `env` output is
filtered so the agent can't read what was scrubbed. The shell
inheritance path is otherwise the cheapest credential-
exfiltration channel in the whole design and needs no static
evasion; scrubbing is deterministic, zero-prompt, and composes
with the exfil guardrails instead of relying on them.

## Compound commands

Segment on top-level `&&`, `||`, `;`, `|` (existing tokenizer).
Each segment is classified independently; the command runs only if
every segment clears, and the highest tier among segments wins. A
Tier 1 + Tier 2 mixture prompts once for the whole command line.

**Wrapper unwrapping.** Interpreters and exec wrappers —
`bash`, `sh`, `zsh -c`, `env`, `nice`, `timeout`, `nohup`, bare
`xargs` — are unwrapped recursively before classification: the
real command (for `-c`-style shells, the parsed string) is what
gets tiered. `bash killall Dock` is a `killall`, never a Tier 1
"shell". A wrapper the unwrapper cannot see through drops to the
prompt tier. Without this, `bash <anything>` is an unclimbable
blind spot in the table.

## Configuration

`sandbox.json` v2 schema:

```json
{
  "version": 2,
  "rules": {
    "allow": ["cargo test *", "git push origin *"],
    "deny":   ["rm * ~"]
  }
}
```

- `allow` → Tier 1; `deny` → prompt with rule name given as reason.
- Allow/deny rules are command-prefix globs, human-editable, and
  carry higher provenance than the table: a deny beats both, an
  allow beats a Tier 2 verdict, neither beats a guardrail.
- Guardrails are hardcoded, not config — the floor the config
  cannot lower.
- Migration: load v1 `sandbox.json` + `approvals.json` once;
  existing global grants become `allow` prefix rules; v1 fields
  (`mode`, `network`, `extra_writable`, `extra_readonly`) are
  dropped. Missing file = defaults.
- Concurrency: many atom clients/servers share one `sandbox.json`.
  Rule writes go through a single-writer lock and atomic
  temp-file + rename; writers re-read before append so concurrent
  sessions never drop each other's rules.

## Implementation map

- `atom-sandbox`: delete `seatbelt.rs` and the `confined` field
  from `ExecOutcome`; `policy.rs` collapses to
  `SandboxConfig { rules: { allow, deny } }`; `rules.rs` widens
  the Allow table to categories A–H; arg-veto lists drop
  matched-flag commands to the prompt tier.
- `atom-sandbox/exec.rs`: the pipeline becomes
  `analyze → guardrail floor → approval gate → spawn`; Seatbelt
  paths deleted; environment scrubbing happens in `spawn_*`
  helpers.
- `atom-sandbox/approvals.rs`: `Decision` loses `AllowSession`;
  `AllowOnce`/`AllowAll`/`DenyOnce`/`DenyAll`; global grants are
  the only persisted ones, written through the same lock as
  config rules.
- `atom-tui`: `approval_buttons()` switches to `[y]/[a]/[n]/[d]`
  + Esc; the prefix-rule preview pane and help line are new
  blocks in the approval render path; verdict display notes the
  matched rule id and tier origin (guardrail / arg-veto /
  unknown-command / rule-name).
- `atom-server`: `approval_request_event` carries an `origin`
  field so the chrome can attribute the prompt; the decision
  router accepts the four-button decision set.

## Open questions (pre-implementation)

1. Exact command lists per category — the above is the spec
   outline; the implementation table will pin the entries and
   their arg-any / arg-all constraints.
2. Prefix-rule generalization algorithm on accept-all: where the
   wildcard lands (two-word default, flag-token exceptions, cap
   for dangerous heads).
3. Whether the prefix-rule preview is inline in the prompt block
   or a sub-step the user navigates into.
4. Audit-log retention and rotation policy.
5. Whether `osascript` / `launchctl` mutating forms deserve a
   separate "automation" category with a softer floor (prompt
   instead of deny), since power users routinely call these.

## Residual risks (accepted, by posture decision)

v2 removes Seatbelt and keeps network + installs unconfined.
These follow from that and are documented, not unknown:

1. **Creative exfiltration via Tier 1.** Reads are Tier 1,
   network is Tier 1, and static shape matching cannot enumerate
   encodings. An injected agent that reads a secret and sends it
   out through a shape the exfil guardrail doesn't list (base64
   indirection, heredoc into an unlisted tool, a Tier 1
   interpreter the table trusts) can exfiltrate data. The two big
   cheap channels are closed — subprocess environments are
   scrubbed and keychain access is guarded — so what's left is
   file content the user can already read, and the guardrail list
   is expected to grow with real cases.
2. **Install-script code execution.** `npm install`/postinstall
   runs arbitrary code at Tier 1, and without confinement it can
   do anything that code wants on first run. Accepted: malicious
   packages are post-hoc reviewable and cannot pre-stage broad
   persistence (rc/PATH/atom-config/.git-hook writes are
   protected paths; kill/automation/keychain tooling is
   guarded).
3. **Prompt fatigue.** The wide table reduces prompts but does
   not eliminate them. Users who reflexively `[a]` on every
   prompt widen Tier 1 toward unsafe shapes. The danger is
   visible in the chrome (prefix preview, dangerous-head flag)
   but not enforced — accept-all is a user choice, not a
   recommendation.
4. **Under-classification of niche ecosystems.** Languages and
   tools the table doesn't list fall to the prompt tier; a
   workflow heavy on, say, `zig build` or `terraform plan` will
   prompt on every invocation. The wide Tier 1 grows with the
   ecosystem over time as users accept-all their common shapes;
   the spec starts with the universal core.
5. **Guardrail miscategorization.** Putting a verb in the
   guardrail list is a one-way door in practice — even deny
   rules written by `[d]` cannot silence it. Decisions like
   `kill` (which has safe subsets — `kill $!` after a forked
   subprocess) and `defaults write` (which is innocuous inside
   the workspace) are honest tradeoffs; the floor costs one
   prompt per legitimate use. Acceptance: friction is the price
   of the floor.