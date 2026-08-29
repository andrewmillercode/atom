# Sandbox v2

A rewrite of how atom gates bash commands. v1 prompts too often. v2 is generous by default, asks only when it has to, and keeps a hard deny floor that no setting can lower. No LLM review step.

Status: spec. Not implemented.

## The idea

The classifier is the rule table plus arg/path analysis. The only humans involved are the agent and the user. That removes the biggest prompt-injection surface in v1 (an LLM reviewer would have read attacker-controlled text) at the cost of more prompts for things the table doesn't recognize. The wide allowlist below is what makes that tradeoff pay off.

```
Static table → Allow verdict  → run silently
Guardrail    → Deny verdict   → blocked
Static table → Ask verdict    → ask the user
User prompt  → once / always / deny once / deny always
```

## Tier 1 — silent allowlist

A command is Tier 1 when every token is statically resolvable (no `$(…)`, backticks, `<(…)`, `$FOO`, unexpanded globs), argv is NFKC-normalized with zero-width chars stripped, and write targets are canonicalized (`/Users/./me` and `..` chains resolve). Flag-vetoed shapes — `find -delete/-exec`, `file -f`, `sort -o`, `sed -i`, `tar -x` — drop to the prompt tier even when their bare command is Tier 1.

Categories:

- **Reads.** No writes, no network. `cat head tail less more nl`, `ls tree eza exa lsd`, `find` (no `-delete/-exec/-ok`), `stat file du df`, `wc sort uniq cut paste tr column fmt tac rev join comm`, `diff cmp patch --dry-run`, `grep rg ag ack fd`, `awk sed` (no `-i`), `jq yq xmllint`, `xxd od strings`, `sha*sum cksum`, `base64 -d`, `which type command hash`, `whoami id groups hostname uname date pwd` and `env printenv` (filtered — see Environment scrubbing).
- **Builds and tests.** Write artifacts to cwd/build dirs. `make ninja meson cmake --build`, `cargo build|check|test|clippy|fmt|doc|run|bench|clean`, `go build|test|run|vet|mod tidy|download`, `python -m pytest|unittest`, `pytest ruff mypy pyright black isort`, `tsc ts-node tsx deno test|check`, `bun test|run|build`, `pnpm run|test|build|exec` (not `publish`), `yarn run|test|build`, `bundle exec rake rspec`, `swift build|test|run|package xcodebuild`, `mix compile|test|run|docs`, `dotnet build|test|run|restore`, `mvn gradle`, `gofmt prettier shellcheck shfmt`.
- **Package installs.** Accepts postinstall risk. `cargo add|update|install`, `npm i|install|add|update|ci` (not `publish`), `pnpm i|install|add|update`, `yarn install|add`, `bun install|add`, `pip pip3 pipx`, `uv pip|add|sync|run`, `poetry install|add|update|run`, `gem install`, `bundle install`, `go get|install`, `brew install|upgrade|reinstall|tap`, `apt|apt-get aptitude install|update|upgrade`, `yum dnf zypper pacman apk add`, `asdf install`, `nix profile install`. Publishes (`cargo publish`, `npm publish`, `dotnet nuget push`) prompt.
- **Network fetches.** Bytes in, no execution. `curl wget http httpie`, `gh api glab api`, `ping traceroute mtr`, `dig host nslookup whois`, `ssh-keyscan`, `git fetch`.
- **Local VCS.** Additive or reversible. `git status|log|diff|show|blame|branch|tag|remote|config --get|reflog|ls-files|worktree|help|version`, `git add`, `git rm --cached`, `git mv`, `git commit` (no `--amend`), `git checkout -b`, `git switch -c`, `git stash|apply|pop`, `git tag`, `git init`, `git revert`, `git merge` (no force), `git rebase` (no force). `git push`, `git reset --hard`, `git clean -fd`, `git branch -D` prompt.
- **Filesystem adds.** Within cwd only. `mkdir -p`, `touch`, `cp ln mv install rsync truncate tee`, `tar -c zip -r gzip bzip2 xz zstd`, `git init`. Writes outside cwd prompt.
- **System read-only.** `ps top -l pgrep -l`, `lsof netstat ss ifconfig ip`, `mount` (no args), `diskutil list|info|apfs list`, `sysctl -n iostat vm_stat`, `uptime launchctl list` (read-only).
- **Dev helpers.** `docker ps|images|logs|inspect|version|info`, `docker compose build|up|down|ps|logs|config`, `kubectl get|describe|logs|version|config view`, `nix build|develop|run|flake update`, `make -n`.

Anything outside the table is Tier 2 (ask). The table is the default; users widen it through accept-all, never the other way.

## Tier 2 — the prompt

Decisions:

- `[y]` accept once — run, no memory.
- `[a]` accept always — run + save a prefix rule to `sandbox.json`.
- `[n]` deny once — error back to the model.
- `[d]` deny always — error now + save a deny rule.
- `[esc]` cancel — error back, nothing saved.

Session-scoped grants are gone. `[a]` writes a **prefix rule** so the command family lands in Tier 1 forever (approving `cargo test --release` saves `cargo test *`); `[d]` writes a deny rule so the family never prompts again.

**Unspoofable prompt.** The prompt block is client-rendered scaffolding that lives in the viewport alongside tool output — the current TUI behavior, kept as-is. Any tool output that lands inside the block has its terminal escapes neutralized, so an injected session can't draw a fake approval row. Key presses always reach the real handler regardless of what output draws nearby.

**Subagent prompts.** When the prompt comes from a dispatched subagent, the parent view surfaces it inline with `from_subagent` + `child_title`. The child waits indefinitely; cancel propagates up.

**Accept-all preview.** `[a]` shows the resulting prefix before saving ("this would let all `cargo test` invocations run unprompted"). The user can adjust or back out. Dangerous heads (`rm`, `git push`, `sudo`, `chmod -R`, network-to-interpreter shapes) are flagged as a warning, not blocked — accepting still works.

**Help line.** A dim line under the buttons lists every key binding, the prefix preview, and a "press ? for details" hint that expands each decision into its long form in a modal.

**Audit.** Every prompt and decision is logged. `[a]` and `[d]` lines include the rule that was saved, so grants and denials can be reviewed.

## Guardrails — the deny floor

Hardcoded patterns that override every other decision. A match is blocked outright, no prompt, even for a command the user already accepted. This is what makes the wide table safe.

- Recursive deletes on `$HOME` or system roots: `rm -rf /`, `rm -rf ~`, fork-bomb `:(){ :|:& };:`
- Disk-level: `dd of=/dev/*`, `mkfs*`, `diskutil erase*|apfs|hfs`, `mount` with a device
- Privilege escalation: `sudo su doas pfexec csrutil nvram pmset kextload spctl installer dscl`
- Process kill (any variant, any target): `kill killall pkill` — a wrong name takes down the session or system daemons; there is no safe subset
- System automation: `osascript`, mutating `launchctl`, `crontab`, `defaults write` outside the workspace, `shutdown reboot halt poweroff init telinit`
- Credential exfil: network tools with file payloads (`curl|nc -d @…`, `-T`, `--data-binary`, `<` redirections) referencing `$HOME` or protected paths; piping `~/.ssh/*`, `.env`, or keychain content into network tools
- Keychain: `security dump-keychain`, `security find-*`
- Network-to-interpreter: `curl … | sh|bash|zsh|python`, `wget -O - … | sh`
- Path-escape writes: `touch mkdir cp mv rm ln tee truncate install rsync dd` targeting `/System`, `/bin`, `/sbin`, `/usr`, `/etc`, `/private/etc`, `/boot`, or any symlink that resolves there

`[a]` and prefix rules never reduce a guardrail. Only a deny rule written by `[d]` can silence one, and only deliberately.

## Protected paths

No tier may write these without a prompt. This is the anti-self-escalation floor — an agent that can edit its own gate owns the gate.

- atom's own state: `dataDir()` (`sandbox.json`, `approvals.json`, audit log) and the atom config dir
- Shell startup files: `~/.zshrc`, `~/.zshenv`, `~/.zprofile`, `~/.bashrc`, `~/.bash_profile`, fish config
- Anything on `$PATH`: `~/.local/bin`, `/usr/local/bin`, `/opt/homebrew/bin`, and any other `PATH` entry — a Tier 1 `cp` of a shim over a trusted binary turns later Tier 1 runs into attacker code
- `~/.ssh` (writes; reads stay per the open posture)
- `.git/hooks/` in any repo the agent touches — a planted hook turns every later `git commit` into host-level code execution

Symlinks are checked: a write whose destination or any path component resolves into a protected tree is protected, wherever the link lives. Bash writes to protected paths escalate to the prompt tier; the file tools enforce the same floor so `edit_file` on `sandbox.json` can't bypass the gate.

## Environment scrubbing

Subprocess env vars are scrubbed before each command: provider credentials (`ANTHROPIC_*`, `OPENAI_*`, `GITHUB_TOKEN`) and anything matching `*_TOKEN`, `*_KEY`, `*_SECRET`, `*_PASSWORD` is unset. `printenv` and `env` output is filtered so the agent can't read what was scrubbed. This is the cheapest credential-exfiltration channel in the design — no static evasion needed; scrubbing is deterministic, zero-prompt, and composes with the exfil guardrails.

## Compound commands

Segment on top-level `&&`, `||`, `;`, `|`. Each segment is classified independently; the command runs only if every segment clears, and the highest tier among segments wins. A Tier 1 + Tier 2 mix prompts once for the whole line.

Wrappers (`bash sh zsh -c env nice timeout nohup` and bare `xargs`) are unwrapped recursively before classification. `bash killall Dock` is a `killall`, never a "shell". A wrapper the unwrapper can't see through drops to the prompt tier — without this, `bash <anything>` is an unclimbable blind spot.

## Configuration

```json
{
  "version": 2,
  "rules": {
    "allow": ["cargo test *", "git push origin *"],
    "deny":   ["rm * ~"]
  }
}
```

- `allow` → Tier 1; `deny` → prompt with the rule name as the reason.
- Allow/deny rules are command-prefix globs, human-editable. A deny beats both; an allow beats Tier 2; neither beats a guardrail.
- Guardrails are hardcoded, not config — the floor the config cannot lower.
- Migration: load v1 `sandbox.json` + `approvals.json` once; existing global grants become `allow` prefix rules; v1 fields (`mode`, `network`, `extra_writable`, `extra_readonly`) are dropped. Missing file = defaults.
- Concurrency: many clients share one `sandbox.json`. Rule writes go through a single-writer lock with atomic temp-file + rename; writers re-read before append so concurrent sessions don't drop each other's rules.

## What changes in code

- **`atom-sandbox`**: drop `seatbelt.rs` and the `confined` field on `ExecOutcome`. `policy.rs` becomes `SandboxConfig { rules: { allow, deny } }`. `rules.rs` widens the Allow table to the eight categories. Arg-veto lists drop matched-flag commands to the prompt tier.
- **`atom-sandbox/exec.rs`**: pipeline becomes `analyze → guardrail floor → approval gate → spawn`. Seatbelt paths deleted. Environment scrubbing lives in `spawn_*` helpers.
- **`atom-sandbox/approvals.rs`**: `Decision` loses `AllowSession`; becomes `AllowOnce`/`AllowAll`/`DenyOnce`/`DenyAll`. Global grants are the only persisted state, written through the same lock as config rules.
- **`atom-tui`**: `approval_buttons()` switches to `[y]/[a]/[n]/[d]` + Esc. Prefix-rule preview and help line are new blocks in the approval render path. Verdict display notes the matched rule id and tier origin.
- **`atom-server`**: `approval_request_event` carries an `origin` field. The decision router accepts the four-button decision set.

## Open questions

1. Exact command lists per category — this is the spec outline; the implementation table will pin entries and their arg-any / arg-all constraints.
2. Where the wildcard lands in a prefix rule on accept-all (two-word default, flag-token exceptions, cap for dangerous heads).
3. Whether the prefix-rule preview is inline or a sub-step.
4. Audit-log retention and rotation.
5. Whether `osascript` / mutating `launchctl` deserve a softer floor (prompt instead of deny) for power users.

## Residual risks

Accepted by posture, not unknown:

1. **Creative exfiltration via Tier 1.** Reads are Tier 1, network is Tier 1, and static shape matching can't enumerate encodings. An injected agent could read a secret and send it through a shape the exfil guardrail doesn't list (base64 indirection, heredoc into an unlisted tool, a trusted Tier 1 interpreter). The two cheap channels are closed (subprocess env scrubbed, keychain guarded), so what's left is file content the user can already read. The guardrail list grows with real cases.
2. **Install-script code execution.** `npm install`/postinstall runs arbitrary code at Tier 1, unconfined. Malicious packages are post-hoc reviewable and can't pre-stage broad persistence — rc/PATH/atom-config/.git-hook writes are protected, kill/automation/keychain tooling is guarded.
3. **Prompt fatigue.** The wide table reduces prompts but doesn't eliminate them. Users who reflexively `[a]` widen Tier 1 toward unsafe shapes. The danger is visible in the chrome (prefix preview, dangerous-head flag) but not enforced.
4. **Under-classification of niche ecosystems.** Languages and tools not in the table prompt every time. The wide Tier 1 grows as users accept-all their common shapes; the spec starts with the universal core.
5. **Guardrail miscategorization.** Putting a verb in the guardrail list is a one-way door — even deny rules from `[d]` can't silence it. `kill` has safe subsets (`kill $!`); `defaults write` is innocuous inside the workspace. The floor costs one prompt per legitimate use; friction is the price.