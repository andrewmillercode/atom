//! Layer 1 — static analysis + permission rules over shell commands.
//!
//! Commands are tokenized (quotes/escapes aware) and split into argv
//! segments at top-level `&& || ; | &`. Each segment runs through the
//! built-in rule table plus a path scan; the worst verdict wins
//! (Deny > Ask > Allow), unknown commands default to Ask.

use glob::Pattern;
use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// How a single command segment is judged.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum Verdict {
    #[default]
    #[serde(rename = "allow")]
    Allow,
    #[serde(rename = "ask")]
    Ask,
    #[serde(rename = "deny")]
    Deny,
}

impl Verdict {
    pub fn as_str(&self) -> &'static str {
        match self {
            Verdict::Allow => "allow",
            Verdict::Ask => "ask",
            Verdict::Deny => "deny",
        }
    }
}

impl std::fmt::Display for Verdict {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Result of analyzing one shell command line.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Analysis {
    pub verdict: Verdict,
    /// Rule ids that fired across all segments (plus synthetic ids like
    /// "unknown-command", "path-escape-write", "git-hooks-write").
    pub matched_rules: Vec<String>,
    /// Tokenized argv segments (top-level && || ; | & splits).
    pub segments: Vec<Vec<String>>,
    /// Any resolved argument path lands under $HOME.
    pub touches_home: bool,
    /// A write-looking context targets something under .git/hooks/.
    pub writes_git_hooks: bool,
    /// A segment uses a network tool or package manager.
    pub uses_network: bool,
    /// Some argument resolves outside workspace_root.
    pub paths_outside_workspace: bool,
    /// Short human-readable explanation of which tier produced the
    /// verdict (e.g. "static allowlist", "arg veto", "guardrail",
    /// "unknown command"). Surfaced in the approval prompt so the user
    /// knows why the table landed where it did.
    pub tier_origin: String,
}

/// One built-in permission rule. Matchers are globs evaluated against
/// argv[0] (both basename and full form), arguments, and arity.
#[derive(Debug)]
pub struct Rule {
    pub id: &'static str,
    pub reason: &'static str,
    pub verdict: Verdict,
    /// Glob(s) for the program name; "" matches any program. A pattern
    /// containing '/' matches the full argv[0], otherwise the basename.
    pub prog: &'static str,
    /// At least one of these globs must match at least one argument
    /// (after argv[0]). Empty = unconstrained.
    pub arg_any: &'static [&'static str],
    /// Every glob here must match at least one argument each.
    pub arg_all: &'static [&'static str],
    /// Minimum argument count after argv[0].
    pub min_args: usize,
    /// Maximum argument count after argv[0] (usize::MAX = unbounded).
    pub max_args: usize,
    /// Rule involves network egress (sets Analysis::uses_network).
    pub network: bool,
}

macro_rules! rule {
    ($id:expr, $reason:expr, $verdict:expr, $prog:expr) => {
        Rule {
            id: $id,
            reason: $reason,
            verdict: $verdict,
            prog: $prog,
            arg_any: &[],
            arg_all: &[],
            min_args: 0,
            max_args: usize::MAX,
            network: false,
        }
    };
    ($id:expr, $reason:expr, $verdict:expr, $prog:expr, any: [$($a:expr),*]) => {
        Rule {
            id: $id,
            reason: $reason,
            verdict: $verdict,
            prog: $prog,
            arg_any: &[$($a),*],
            arg_all: &[],
            min_args: 0,
            max_args: usize::MAX,
            network: false,
        }
    };
    ($id:expr, $reason:expr, $verdict:expr, $prog:expr, net: true) => {
        Rule {
            id: $id,
            reason: $reason,
            verdict: $verdict,
            prog: $prog,
            arg_any: &[],
            arg_all: &[],
            min_args: 0,
            max_args: usize::MAX,
            network: true,
        }
    };
    ($id:expr, $reason:expr, $verdict:expr, $prog:expr, any: [$($a:expr),*], net: true) => {
        Rule {
            id: $id,
            reason: $reason,
            verdict: $verdict,
            prog: $prog,
            arg_any: &[$($a),*],
            arg_all: &[],
            min_args: 0,
            max_args: usize::MAX,
            network: true,
        }
    };
    ($id:expr, $reason:expr, $verdict:expr, $prog:expr, all: [$($a:expr),*]) => {
        Rule {
            id: $id,
            reason: $reason,
            verdict: $verdict,
            prog: $prog,
            arg_any: &[],
            arg_all: &[$($a),*],
            min_args: 0,
            max_args: usize::MAX,
            network: false,
        }
    };
    ($id:expr, $reason:expr, $verdict:expr, $prog:expr, any: [$($a:expr),*], all: [$($b:expr),*]) => {
        Rule {
            id: $id,
            reason: $reason,
            verdict: $verdict,
            prog: $prog,
            arg_any: &[$($a),*],
            arg_all: &[$($b),*],
            min_args: 0,
            max_args: usize::MAX,
            network: false,
        }
    };
    ($id:expr, $reason:expr, $verdict:expr, $prog:expr, min: $min:expr) => {
        Rule {
            id: $id,
            reason: $reason,
            verdict: $verdict,
            prog: $prog,
            arg_any: &[],
            arg_all: &[],
            min_args: $min,
            max_args: usize::MAX,
            network: false,
        }
    };
}

/// The built-in rule table, ordered deny -> ask -> allow. All matching
/// rules fire per segment; the aggregated verdict takes the worst.
pub static RULES: &[Rule] = &[
    // --- destructive / system tampering: Deny ---
    rule!("rm-root", "removes the filesystem root", Verdict::Deny, "rm",
          any: ["/", "/*"]),
    rule!("rm-home", "recursively deletes the home directory", Verdict::Deny, "rm",
          any: ["~", "~/*", "$HOME", "$HOME/*"]),
    rule!(
        "fork-bomb",
        "classic fork-bomb shape (:(){ :|:& };:)",
        Verdict::Deny,
        ":"
    ),
    rule!(
        "mkfs",
        "creates a filesystem on a raw device",
        Verdict::Deny,
        "mkfs*"
    ),
    rule!("dd-device", "overwrites a raw device via dd of=/dev/*", Verdict::Deny, "dd",
          all: ["of=/dev/*"]),
    rule!(
        "shutdown-family",
        "shuts down or reboots the machine",
        Verdict::Deny,
        "{shutdown,reboot,halt,poweroff,init,telinit}"
    ),
    rule!(
        "privilege-escalation",
        "switches to another user / superuser",
        Verdict::Deny,
        "{sudo,su,doas,pfexec,dzdo}"
    ),
    rule!(
        "launchctl-mutate",
        "manages launchd services",
        Verdict::Deny,
        "launchctl",
        any: ["load", "unload", "bootstrap", "bootout", "enable", "disable",
              "kickstart", "reboot", "start", "stop", "kill", "bless",
              "remove", "submit", "override", "print-cache"]
    ),
    rule!(
        "csrutil",
        "toggles System Integrity Protection",
        Verdict::Deny,
        "csrutil"
    ),
    rule!("nvram", "writes firmware variables", Verdict::Deny, "nvram"),
    rule!(
        "pmset",
        "changes power management settings",
        Verdict::Deny,
        "pmset"
    ),
    rule!(
        "kext",
        "loads or inspects kernel extensions",
        Verdict::Deny,
        "{kextload,kextunload,kextutil}"
    ),
    rule!(
        "installer",
        "runs system package installers",
        Verdict::Deny,
        "installer"
    ),
    rule!("dscl", "edits directory services", Verdict::Deny, "dscl"),
    rule!(
        "spctl",
        "changes Gatekeeper assessment policy",
        Verdict::Deny,
        "spctl"
    ),
    rule!("disk-erase", "erases or reformats disks/volumes", Verdict::Deny,
          "diskutil", any: ["erase*", "apfs*create*", "apfs*delete*",
                            "apfs*resize*", "apfs*add*", "apfs*erase*",
                            "hfs*create*"]),
    rule!(
        "mount-device",
        "mounts/unmounts filesystems",
        Verdict::Ask,
        "{mount,umount}"
    ),
    rule!(
        "osascript",
        "runs AppleScript automation",
        Verdict::Deny,
        "osascript"
    ),
    rule!(
        "crontab",
        "edits the system crontab",
        Verdict::Deny,
        "crontab"
    ),
    rule!(
        "kill",
        "signals arbitrary processes (no safe subset)",
        Verdict::Deny,
        "{kill,killall,pkill}"
    ),
    rule!(
        "security-keychain",
        "inspects the system keychain",
        Verdict::Deny,
        "security",
        any: ["dump-keychain", "find-*", "add-*", "delete-*", "set-*"]
    ),
    rule!("system-path-write", "writes into system directories", Verdict::Deny,
          "{rm,mv,cp,chmod,chown,chgrp,ln,mkdir,touch,tee,truncate,install,rsync,dd}",
          any: ["/System/*", "/bin/*", "/sbin/*", "/usr/*", "/etc/*",
                "/private/etc/*", "/boot/*"]),
    // --- network tools and package managers: Ask ---
    rule!("curl", "transfers data over the network", Verdict::Ask, "{curl,curlie,httpie}",
          net: true),
    rule!("wget", "downloads from the network", Verdict::Ask, "wget*", net: true),
    rule!("netcat", "raw network connections", Verdict::Ask, "{nc,ncat,netcat}",
          net: true),
    rule!("telnet", "plain-text remote sessions", Verdict::Ask, "telnet", net: true),
    rule!("ssh", "remote shell / tunneling", Verdict::Ask, "ssh*", net: true),
    rule!("scp", "copies files over ssh", Verdict::Ask, "scp", net: true),
    rule!("sftp", "transfers files over ssh", Verdict::Ask, "sftp", net: true),
    rule!("ftp", "file transfer protocol clients", Verdict::Ask, "{ftp,lftp}", net: true),
    rule!("rsync", "syncs files (possibly to a remote host)", Verdict::Ask, "rsync",
          net: true),
    rule!("dns-tools", "network lookups", Verdict::Ask, "{dig,nslookup,host}",
          net: true),
    rule!("ping", "probes hosts on the network", Verdict::Ask, "ping", net: true),
    rule!("openssl-client", "openssl can open network connections", Verdict::Ask,
          "openssl", net: true),
    rule!("git-network", "git push/pull/fetch/clone contacts remotes", Verdict::Ask,
          "git", any: ["push", "pull", "fetch", "clone", "lfs", "svn", "p4"],
          net: true),
    rule!("pip-install", "downloads packages from PyPI", Verdict::Ask, "pip*",
          any: ["install", "download", "wheel"], net: true),
    rule!("npm-install-publish", "installs or publishes npm packages", Verdict::Ask,
          "npm", any: ["install", "add", "ci", "update", "publish"], net: true),
    rule!("pnpm-install-publish", "installs or publishes pnpm packages", Verdict::Ask,
          "pnpm", any: ["install", "add", "publish"], net: true),
    rule!("yarn-install-publish", "installs or publishes yarn packages", Verdict::Ask,
          "yarn", any: ["install", "add", "publish"], net: true),
    rule!("cargo-install-publish", "installs or publishes cargo crates", Verdict::Ask,
          "cargo", any: ["install", "publish", "add"], net: true),
    rule!("cargo-build-test", "builds or tests workspace code locally", Verdict::Allow,
          "cargo", any: ["test", "build", "check", "clippy", "doc", "fmt", "clean", "run"]),
    rule!("brew-install", "installs packages from Homebrew", Verdict::Ask, "brew",
          any: ["install", "upgrade", "reinstall", "tap"], net: true),
    rule!("gem-install", "installs gems from RubyGems", Verdict::Ask, "gem",
          any: ["install", "update"], net: true),
    rule!("apt-install", "installs system packages", Verdict::Ask,
          "{apt,apt-get,aptitude}", any: ["install", "upgrade", "update", "full-upgrade"],
          net: true),
    rule!("rpm-family-install", "installs system packages", Verdict::Ask,
          "{yum,dnf,zypper}", any: ["install", "update", "upgrade"], net: true),
    rule!("pacman-install", "installs system packages", Verdict::Ask, "pacman",
          any: ["-S", "-Sy", "-Syu", "-U"], net: true),
    rule!("apk-add", "installs system packages", Verdict::Ask, "apk", any: ["add"],
          net: true),
    rule!("go-install", "downloads Go modules or binaries", Verdict::Ask, "go",
          any: ["install", "get"], net: true),
    rule!("conda-install", "installs conda packages", Verdict::Ask, "conda",
          any: ["install", "update"], net: true),
    // --- arbitrary code / mutating utilities: Ask ---
    rule!(
        "interpreters",
        "executes arbitrary interpreter code",
        Verdict::Ask,
        "{python,python3,pipenv,poetry,node,deno,bun,perl,ruby,php,lua,julia,R,Rscript}"
    ),
    rule!(
        "shell-nested",
        "spawns an interactive/nested shell",
        Verdict::Ask,
        "{bash,sh,zsh,dash,ksh,fish}"
    ),
    rule!(
        "script-execution",
        "runs an unreviewed script file",
        Verdict::Ask,
        "*.sh"
    ),
    rule!("find-mutating", "find -delete/-exec mutates files", Verdict::Ask, "find",
          any: ["-delete", "-exec", "-execdir", "-ok", "-okdir"]),
    rule!("sed-inplace", "sed -i rewrites files in place", Verdict::Ask, "sed",
          any: ["-i", "-i*"]),
    rule!("archive-extract", "extracts archives onto disk", Verdict::Ask,
          "{tar,gtar}", any: ["-x", "--extract", "-x*", "--extract*"]),
    rule!(
        "unzip-extract",
        "extracts zip archives onto disk",
        Verdict::Ask,
        "unzip"
    ),
    rule!("git-config", "git config can redirect hooks (core.hooksPath)", Verdict::Ask,
          "git", any: ["config"]),
    // --- safe read-only / workspace-local: Allow ---
    rule!(
        "list-dir",
        "lists directories",
        Verdict::Allow,
        "{ls,tree,eza,exa,lsd}"
    ),
    rule!(
        "read-file",
        "prints file contents",
        Verdict::Allow,
        "{cat,bat}"
    ),
    rule!(
        "head-tail",
        "prints file heads/tails",
        Verdict::Allow,
        "{head,tail}"
    ),
    rule!(
        "word-count",
        "counts lines/words/bytes",
        Verdict::Allow,
        "wc"
    ),
    rule!(
        "file-info",
        "inspects file types/metadata",
        Verdict::Allow,
        "{file,stat}"
    ),
    rule!(
        "disk-usage",
        "reports disk usage",
        Verdict::Allow,
        "{du,df}"
    ),
    rule!(
        "locate-binary",
        "locates commands",
        Verdict::Allow,
        "{which,type,whereis,whence,command}"
    ),
    rule!(
        "identity-info",
        "prints user/system identity",
        Verdict::Allow,
        "{whoami,id,groups,hostname,uname,arch,sw_vers,date,pwd,locale,true,false,sleep,test}"
    ),
    rule!("print-text", "echoes text", Verdict::Allow, "{echo,printf}"),
    rule!(
        "grep-search",
        "searches file contents",
        Verdict::Allow,
        "{grep,egrep,fgrep,zgrep}"
    ),
    rule!(
        "fast-search",
        "searches file contents",
        Verdict::Allow,
        "{rg,ag,ack,fd,fdfind}"
    ),
    rule!(
        "diff-files",
        "compares files",
        Verdict::Allow,
        "{diff,cmp,comm,colordiff}"
    ),
    rule!(
        "text-process",
        "pure text transformation",
        Verdict::Allow,
        "{sort,uniq,cut,paste,tr,column,fold,fmt,nl,tac,rev,join}"
    ),
    rule!(
        "checksums",
        "computes checksums",
        Verdict::Allow,
        "{md5,md5sum,shasum,sha1sum,sha256sum,sha512sum,cksum}"
    ),
    rule!(
        "encoders",
        "encodes/decodes data locally",
        Verdict::Allow,
        "{base64,xxd,od,hexdump,strings}"
    ),
    rule!(
        "json-query",
        "queries structured data locally",
        Verdict::Allow,
        "{jq,yq,xmllint}"
    ),
    rule!("find-read", "searches filenames", Verdict::Allow, "find", min: 1),
    rule!("sed-read", "stream-edits without -i", Verdict::Allow, "sed", min: 1),
    rule!("git-read", "reads repository state", Verdict::Allow, "git",
          any: ["status", "log", "diff", "show", "blame", "branch", "tag",
                "rev-parse", "describe", "reflog", "ls-files", "shortlog",
                "worktree", "help", "version"]),
    rule!("tar-list", "lists archive contents", Verdict::Allow, "{tar,gtar}",
          any: ["-t", "--list", "-t*", "tf"]),
    rule!("unzip-list", "lists zip contents", Verdict::Allow, "unzip",
          any: ["-l", "-Z"]),
    rule!(
        "create-file",
        "creates empty files",
        Verdict::Allow,
        "touch"
    ),
    rule!("mkdir", "creates directories", Verdict::Allow, "mkdir"),
    rule!(
        "rmdir",
        "removes empty directories",
        Verdict::Allow,
        "rmdir"
    ),
    rule!("copy-files", "copies files within the workspace", Verdict::Allow, "cp",
          min: 2),
    rule!("move-files", "moves files within the workspace", Verdict::Allow, "mv",
          min: 2),
    rule!("link-files", "links files within the workspace", Verdict::Allow, "ln",
          min: 2),
    rule!("tee-pipe", "tees piped output to a file", Verdict::Allow, "tee", min: 1),
    rule!("truncate-file", "resizes a file", Verdict::Allow, "truncate", min: 1),
    rule!("remove-files", "removes workspace files", Verdict::Allow, "rm", min: 1),
    rule!("chmod", "changes permissions (needs mode + path)", Verdict::Allow, "chmod",
          min: 2),
    rule!("chown", "changes ownership (needs owner + path)", Verdict::Allow, "chown",
          min: 2),
    rule!("install-files", "copies files into place", Verdict::Allow, "install",
          min: 2),
    // --- v2 allowlist: builds & test runners (Category 2) ---
    rule!("cargo-test-bench", "runs cargo benchmarks", Verdict::Allow, "cargo",
          any: ["bench"]),
    rule!("go-test-run", "runs Go programs", Verdict::Allow, "go",
          any: ["test", "run", "vet", "mod", "download", "env"]),
    rule!("pytest", "runs Python tests", Verdict::Allow, "pytest"),
    rule!(
        "py-formatters",
        "formats Python source",
        Verdict::Allow,
        "{ruff,black,isort,mypy,pyright,flake8}"
    ),
    rule!(
        "js-ts-tools",
        "runs JS/TS toolchain",
        Verdict::Allow,
        "{tsc,ts-node,tsx,node}"
    ),
    rule!("bun-test", "runs bun tests/build", Verdict::Allow, "bun",
          any: ["test", "run", "build"]),
    rule!("pnpm-run", "runs pnpm scripts", Verdict::Allow, "pnpm",
          any: ["run", "test", "build", "exec", "dlx"]),
    rule!("yarn-run", "runs yarn scripts", Verdict::Allow, "yarn",
          any: ["run", "test", "build"]),
    rule!("bundle-exec", "runs bundle exec wrappers", Verdict::Allow, "bundle",
          any: ["exec"]),
    rule!(
        "rake-rspec",
        "runs rake/rspec",
        Verdict::Allow,
        "{rake,rspec}"
    ),
    rule!("swift-tooling", "builds Swift packages", Verdict::Allow,
          "{swift,xcodebuild}", any: ["build", "test", "run", "package"]),
    rule!("elixir-mix", "runs mix tasks", Verdict::Allow, "mix",
          any: ["compile", "test", "run", "docs"]),
    rule!("dotnet-build", "builds dotnet projects", Verdict::Allow, "dotnet",
          any: ["build", "test", "run", "restore"]),
    rule!(
        "maven-gradle",
        "builds Java/Kotlin projects",
        Verdict::Allow,
        "{mvn,mvnw,gradle,gradlew}"
    ),
    rule!(
        "js-formatters",
        "formats JS/TS source",
        Verdict::Allow,
        "{prettier,gofmt,shellcheck,shfmt}"
    ),
    rule!(
        "make-build",
        "runs make targets",
        Verdict::Allow,
        "{make,ninja,meson,cmake}"
    ),
    // --- v2 allowlist: narrow network shapes (Category 4) ---
    rule!("gh-api", "calls GitHub API", Verdict::Ask, "gh", any: ["api"],
          net: true),
    rule!("glab-api", "calls GitLab API", Verdict::Ask, "glab", any: ["api"],
          net: true),
    rule!("traceroute", "probes network routes", Verdict::Ask,
          "{traceroute,mtr}", net: true),
    rule!("whois", "looks up WHOIS records", Verdict::Ask, "whois", net: true),
    rule!("ssh-keyscan", "scans SSH host keys", Verdict::Ask, "ssh-keyscan",
          net: true),
    // --- v2 allowlist: local VCS additions (Category 5) ---
    rule!("git-add", "stages files", Verdict::Allow, "git", any: ["add"]),
    rule!("git-rm-cached", "removes from index, keeps file on disk",
          Verdict::Allow, "git", any: ["rm"], all: ["--cached"]),
    rule!("git-mv", "moves files inside the index", Verdict::Allow, "git",
          any: ["mv"]),
    rule!("git-commit", "commits staged changes", Verdict::Allow, "git",
          any: ["commit"]),
    rule!("git-checkout-new", "creates a new branch", Verdict::Allow, "git",
          any: ["checkout"], all: ["-b"]),
    rule!("git-switch-new", "creates a new branch via switch", Verdict::Allow,
          "git", any: ["switch"], all: ["-c"]),
    rule!("git-stash", "stashes/unstashes working tree", Verdict::Allow, "git",
          any: ["stash", "apply", "pop"]),
    rule!("git-tag-add", "creates a tag", Verdict::Allow, "git", any: ["tag"]),
    rule!("git-init", "initializes a repository", Verdict::Allow, "git",
          any: ["init"]),
    rule!("git-revert", "reverts a commit (reversible)", Verdict::Allow, "git",
          any: ["revert"]),
    rule!("git-config-get", "reads git config", Verdict::Allow, "git",
          any: ["config"], all: ["--get"]),
    // --- v2 allowlist: filesystem creation (Category 6) ---
    rule!("mkdir-p", "creates directories", Verdict::Allow, "mkdir",
          any: ["-p"]),
    rule!("zip-create", "creates zip archives", Verdict::Allow, "zip",
          any: ["-r", "-r*"]),
    rule!("tar-create", "creates tar archives", Verdict::Allow, "{tar,gtar}",
          any: ["-c", "--create", "-c*", "--create*", "cf"]),
    rule!(
        "compress",
        "compresses files",
        Verdict::Allow,
        "{gzip,bzip2,xz,zstd}"
    ),
    // --- v2 allowlist: system read-only (Category 7) ---
    rule!("ps-top", "lists processes", Verdict::Allow, "{ps,top,pgrep}",
          any: ["-l", "-p", "-ef", "-ax", "-axo"]),
    rule!(
        "net-readonly",
        "inspects network state",
        Verdict::Allow,
        "{lsof,netstat,ss,ifconfig,ip}"
    ),
    rule!("diskutil-list", "lists disks/volumes", Verdict::Allow, "diskutil",
          any: ["list", "info", "apfs"]),
    rule!("sysctl-n", "reads sysctl values", Verdict::Allow, "sysctl",
          any: ["-n"]),
    rule!(
        "vmstat",
        "reads vm stats",
        Verdict::Allow,
        "{iostat,vm_stat}"
    ),
    rule!("uptime", "shows uptime", Verdict::Allow, "uptime"),
    rule!("launchctl-list", "lists launchd services", Verdict::Allow, "launchctl",
          any: ["list"]),
    // --- v2 allowlist: dev helpers (Category 8) ---
    rule!("docker-ps", "lists containers/images", Verdict::Allow, "docker",
          any: ["ps", "images", "logs", "inspect", "version", "info"]),
    rule!("kubectl-get", "inspects kubernetes resources", Verdict::Allow,
          "kubectl", any: ["get", "describe", "logs", "version"]),
    rule!("kubectl-config-view", "views kubeconfig", Verdict::Allow, "kubectl",
          any: ["config"], all: ["view"]),
    rule!("nix-build", "builds nix derivations", Verdict::Allow, "nix",
          any: ["build", "develop", "run", "flake"]),
    rule!("make-dry-run", "drys runs a makefile", Verdict::Allow, "make",
          any: ["-n", "--dry-run"]),
];

struct CompiledRule {
    rule: &'static Rule,
    progs: Vec<Pattern>,
    any: Vec<Pattern>,
    all: Vec<Pattern>,
}

/// Expand `{a,b,c}` alternatives (glob::Pattern does not apply braces in
/// `matches`, so we do it ourselves). Handles nested groups recursively.
fn expand_braces(pat: &str) -> Vec<String> {
    let Some(open) = pat.find('{') else {
        return vec![pat.to_string()];
    };
    let Some(close_rel) = pat[open..].find('}') else {
        return vec![pat.to_string()];
    };
    let close = open + close_rel;
    let head = &pat[..open];
    let tail = &pat[close + 1..];
    let mut out = Vec::new();
    for alt in pat[open + 1..close].split(',') {
        out.extend(expand_braces(&format!("{head}{alt}{tail}")));
    }
    out
}

fn compile(patterns: &[&str]) -> Vec<Pattern> {
    patterns
        .iter()
        .flat_map(|p| expand_braces(p))
        .map(|p| Pattern::new(&p).expect("curated glob pattern"))
        .collect()
}

static COMPILED_RULES: Lazy<Vec<CompiledRule>> = Lazy::new(|| {
    RULES
        .iter()
        .map(|rule| CompiledRule {
            progs: if rule.prog.is_empty() {
                Vec::new()
            } else {
                compile(&[rule.prog])
            },
            any: compile(rule.arg_any),
            all: compile(rule.arg_all),
            rule,
        })
        .collect()
});

/// Number of rules in the built-in table.
pub fn rule_count() -> usize {
    RULES.len()
}

fn prog_matches(rule: &CompiledRule, argv0: &str) -> bool {
    if rule.progs.is_empty() {
        return true;
    }
    let base = Path::new(argv0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(argv0);
    let base_hit = rule.progs.iter().any(|p| p.matches(base));
    if base_hit {
        return true;
    }
    // Absolute/relative path form: match the whole token against path-y
    // patterns only (patterns containing '/').
    if argv0.contains('/') {
        return rule
            .progs
            .iter()
            .any(|p| p.as_str().contains('/') && p.matches(argv0));
    }
    false
}

fn args_match(rule: &CompiledRule, args: &[String]) -> bool {
    let any_ok = rule.any.is_empty() || rule.any.iter().any(|p| args.iter().any(|a| p.matches(a)));
    if !any_ok {
        return false;
    }
    rule.all.iter().all(|p| args.iter().any(|a| p.matches(a)))
}

/// Programs whose positional arguments are write targets when they resolve
/// outside the workspace.
pub const WRITE_PROGS: &[&str] = &[
    "touch", "mkdir", "cp", "mv", "rm", "ln", "tee", "truncate", "install", "dd", "rsync", "unzip",
    "tar", "patch", "rmdir",
];

pub const NETWORK_BASENAMES: &[&str] = &[
    "curl", "wget", "nc", "ncat", "netcat", "telnet", "ssh", "scp", "sftp", "ftp", "lftp", "rsync",
    "dig", "nslookup", "host", "ping",
];

pub const SHELLS: &[&str] = &["bash", "sh", "zsh", "dash", "ksh", "fish"];

/// Programs considered dangerous: accept-all prefix rules are capped
/// at one word so `[a]` on `rm -rf /tmp/foo` creates the wide `rm *`
/// rule instead of a narrow `rm -rf /tmp/foo *` that wouldn't catch
/// the next `rm -rf /var/log` shape.
pub fn dangerous_heads(head: &str) -> bool {
    matches!(
        head,
        "rm" | "sudo"
            | "su"
            | "doas"
            | "pfexec"
            | "chmod"
            | "chown"
            | "chgrp"
            | "dd"
            | "mkfs"
            | "shutdown"
            | "reboot"
            | "halt"
            | "poweroff"
            | "init"
            | "telinit"
            | "kill"
            | "killall"
            | "pkill"
            | "launchctl"
            | "csrutil"
            | "nvram"
            | "pmset"
            | "kextload"
            | "kextunload"
            | "kextutil"
            | "spctl"
            | "installer"
            | "dscl"
            | "diskutil"
            | "mount"
            | "umount"
            | "osascript"
            | "crontab"
            | "defaults"
    )
}

/// Whitespace-tokenize a command for prefix-rule construction. Stops
/// at shell metacharacters (`&&`, `|`, `;`, `>`). Used by
/// `policy::prefix_for_command`.
pub fn tokenize_for_prefix(cmd: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut started = false;
    for c in cmd.chars() {
        if c.is_whitespace() {
            if started {
                out.push(std::mem::take(&mut cur));
                started = false;
            }
            continue;
        }
        if matches!(c, '&' | '|' | ';' | '>' | '<' | '#' | '\n' | '\r') {
            break;
        }
        cur.push(c);
        started = true;
    }
    if started {
        out.push(cur);
    }
    out
}

fn expand_tilde(token: &str, home: Option<&Path>) -> PathBuf {
    if let Some(h) = home {
        if token == "~" || token == "$HOME" {
            return h.to_path_buf();
        }
        if let Some(rest) = token.strip_prefix("~/") {
            return h.join(rest);
        }
        if let Some(rest) = token.strip_prefix("$HOME/") {
            return h.join(rest);
        }
    }
    PathBuf::from(token)
}

/// Lexically normalize `.`/`..`/duplicate separators without touching the
/// filesystem (no symlink resolution).
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in p.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn within(path: &Path, base: &Path) -> bool {
    path.starts_with(base)
}

/// True when this argv token is a shell flag (starts with '-' but is not
/// exactly "-" which conventionally means stdin).
fn is_flag(tok: &str) -> bool {
    tok.starts_with('-') && tok != "-"
}

fn looks_like_url(tok: &str) -> bool {
    tok.contains("://")
}

/// Extract the payload of `<shell> [-flags...] <payload>` when a `-c`-style
/// flag is present; returns None for interactive shells.
fn nested_shell_payload(args: &[String]) -> Option<&String> {
    let mut payload = None;
    for (i, a) in args.iter().enumerate() {
        let is_c_flag =
            (a.starts_with('-') && !a.starts_with("--") && a.contains('c')) || a == "--command";
        if is_c_flag {
            // bash uses the last -c payload when repeated.
            payload = args.get(i + 1).or(payload);
        }
    }
    payload
}

/// Analyze a command line using `workspace_root` as both the workspace and
/// the assumed cwd for relative tokens.
pub fn analyze(cmd: &str, workspace_root: &Path) -> Analysis {
    analyze_in(cmd, workspace_root, workspace_root)
}

/// Analyze with explicit cwd for relative-token resolution. `strict`
/// escalates outside-workspace writes from Ask to Deny.
pub fn analyze_in(cmd: &str, workspace_root: &Path, cwd: &Path) -> Analysis {
    let strict = false;
    analyze_full(cmd, workspace_root, cwd, strict)
}

/// Full analysis entry point.
pub fn analyze_full(cmd: &str, workspace_root: &Path, cwd: &Path, strict: bool) -> Analysis {
    let stripped = strip_heredocs(cmd);
    let mut out = Analysis {
        segments: tokenize(&stripped),
        ..Default::default()
    };
    let ws_norm = normalize(workspace_root);
    let cwd_norm = normalize(cwd);
    let home = dirs::home_dir();

    for seg in out.segments.clone() {
        let seg_a = analyze_segment(&seg, &ws_norm, &cwd_norm, home.as_deref(), strict);
        if seg_a.verdict > out.verdict {
            out.verdict = seg_a.verdict;
        }
        for id in seg_a.matched_rules {
            if !out.matched_rules.contains(&id) {
                out.matched_rules.push(id);
            }
        }
        out.touches_home |= seg_a.touches_home;
        out.writes_git_hooks |= seg_a.writes_git_hooks;
        out.uses_network |= seg_a.uses_network;
        out.paths_outside_workspace |= seg_a.paths_outside_workspace;
    }
    out.matched_rules.sort();
    out
}

fn analyze_segment(
    seg: &[String],
    ws: &Path,
    cwd: &Path,
    home: Option<&Path>,
    strict: bool,
) -> Analysis {
    let mut a = Analysis::default();
    if seg.is_empty() {
        return a;
    }

    // Effective argv skips leading FOO=bar assignments for matching.
    let skip: usize = seg.iter().take_while(|t| looks_like_assignment(t)).count();
    let effective: Vec<String> = if skip >= seg.len() {
        seg.to_vec()
    } else {
        seg[skip..].to_vec()
    };

    let argv0 = effective[0].as_str();
    let base = Path::new(argv0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(argv0);

    // Nested shells: recurse into the -c payload instead of judging the
    // wrapper.
    if SHELLS.contains(&base) {
        if let Some(payload) = nested_shell_payload(&effective[1..]) {
            let child = analyze_limited(payload, ws, cwd, home, strict, 3);
            a.verdict = child.verdict;
            a.matched_rules.push("shell-nested".to_string());
            a.matched_rules.extend(child.matched_rules);
            a.touches_home |= child.touches_home;
            a.writes_git_hooks |= child.writes_git_hooks;
            a.uses_network |= child.uses_network;
            a.paths_outside_workspace |= child.paths_outside_workspace;
            return a;
        }
    }

    let args = &effective[1..];
    let mut matched_any_rule = false;
    let mut best_allow_id: Option<&'static str> = None;
    let mut best_ask_id: Option<&'static str> = None;
    let mut best_deny_id: Option<&'static str> = None;

    for cr in COMPILED_RULES.iter() {
        let count = args.len();
        if count < cr.rule.min_args || count > cr.rule.max_args {
            continue;
        }
        if !prog_matches(cr, argv0) || !args_match(cr, args) {
            continue;
        }
        matched_any_rule = true;
        a.matched_rules.push(cr.rule.id.to_string());
        if cr.rule.network {
            a.uses_network = true;
        }
        match cr.rule.verdict {
            Verdict::Deny => {
                best_deny_id = Some(cr.rule.id);
                if cr.rule.verdict > a.verdict {
                    a.verdict = cr.rule.verdict;
                }
            }
            Verdict::Ask => {
                best_ask_id = Some(cr.rule.id);
                if cr.rule.verdict > a.verdict {
                    a.verdict = cr.rule.verdict;
                }
            }
            Verdict::Allow => {
                if best_allow_id.is_none() {
                    best_allow_id = Some(cr.rule.id);
                }
            }
        }
    }

    if NETWORK_BASENAMES.contains(&base) {
        a.uses_network = true;
    }

    // Path scan: resolve every non-flag argument against cwd/home and see
    // whether it escapes the workspace.
    let mut force_write_next = false;
    let mut path_escape = false;
    let mut guardrail = false;
    for tok in effective.iter().map(String::as_str) {
        let mut write_ctx = WRITE_PROGS.contains(&base);

        if tok == ">" || tok == ">>" {
            force_write_next = true;
            continue;
        }
        let mut target = tok;
        if force_write_next {
            force_write_next = false;
            write_ctx = true;
        } else if let Some(gt_pos) = tok.rfind('>') {
            // Inline redirection forms: "2>/dev/null", ">&2", "hi>out.txt".
            let tail = tok[gt_pos + 1..].trim();
            if tail.is_empty() {
                force_write_next = true;
                continue;
            }
            write_ctx = true;
            target = tail;
        }

        if is_flag(tok) || (!write_ctx && looks_like_assignment(tok)) || looks_like_url(target) {
            continue;
        }

        let before = (a.verdict, a.matched_rules.len());
        scan_path_token(target, write_ctx, ws, cwd, home, strict, &mut a);
        if a.matched_rules.len() != before.1 {
            path_escape = a.verdict > before.0;
        }
        if tok.starts_with("/System/")
            || tok.starts_with("/bin/")
            || tok.starts_with("/sbin/")
            || tok.starts_with("/usr/")
            || tok.starts_with("/etc/")
            || tok.starts_with("/private/etc/")
            || tok.starts_with("/boot/")
        {
            guardrail = true;
        }
    }

    if !matched_any_rule && !path_escape && a.matched_rules.is_empty() {
        a.matched_rules.push("unknown-command".to_string());
        a.verdict = Verdict::Ask;
    }
    if a.writes_git_hooks && a.verdict < Verdict::Deny {
        a.verdict = Verdict::Deny;
        guardrail = true;
    }

    a.tier_origin = match a.verdict {
        Verdict::Deny => {
            if guardrail {
                "guardrail".into()
            } else if let Some(id) = best_deny_id {
                format!("static deny ({})", id)
            } else {
                "guardrail".into()
            }
        }
        Verdict::Ask => {
            if path_escape {
                "path escape write".into()
            } else if let Some(id) = best_ask_id {
                format!("static ask ({})", id)
            } else if a.matched_rules.iter().any(|r| r == "unknown-command") {
                "unknown command".into()
            } else {
                "approval needed".into()
            }
        }
        Verdict::Allow => {
            if let Some(id) = best_allow_id {
                format!("static allowlist ({})", id)
            } else {
                "static allowlist".into()
            }
        }
    };

    a
}

/// Recursive analysis with a nesting budget so `bash -c 'bash -c ...'`
/// terminates.
fn analyze_limited(
    cmd: &str,
    ws: &Path,
    cwd: &Path,
    home: Option<&Path>,
    strict: bool,
    depth: u8,
) -> Analysis {
    if depth == 0 {
        return Analysis {
            verdict: Verdict::Ask,
            matched_rules: vec!["nesting-limit".to_string()],
            segments: tokenize(cmd),
            ..Default::default()
        };
    }
    let mut out = Analysis {
        segments: tokenize(cmd),
        ..Default::default()
    };
    for seg in out.segments.clone() {
        // Inline copy of segment logic at reduced depth.
        let seg_a = analyze_segment_limited(&seg, ws, cwd, home, strict, depth);
        if seg_a.verdict > out.verdict {
            out.verdict = seg_a.verdict;
        }
        for id in seg_a.matched_rules {
            if !out.matched_rules.contains(&id) {
                out.matched_rules.push(id);
            }
        }
        out.touches_home |= seg_a.touches_home;
        out.writes_git_hooks |= seg_a.writes_git_hooks;
        out.uses_network |= seg_a.uses_network;
        out.paths_outside_workspace |= seg_a.paths_outside_workspace;
    }
    out
}

fn analyze_segment_limited(
    seg: &[String],
    ws: &Path,
    cwd: &Path,
    home: Option<&Path>,
    strict: bool,
    depth: u8,
) -> Analysis {
    if seg.is_empty() {
        return Analysis::default();
    }
    let argv0 = seg[0].as_str();
    let base = Path::new(argv0)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(argv0);
    if SHELLS.contains(&base) {
        if let Some(payload) = nested_shell_payload(if seg.len() > 1 { &seg[1..] } else { &[] }) {
            let child = analyze_limited(payload, ws, cwd, home, strict, depth - 1);
            let mut a = child;
            a.matched_rules.push("shell-nested".to_string());
            return a;
        }
    }
    // Delegate to the shared machinery for everything else by reusing
    // analyze_segment on this segment (depth no longer matters because
    // recursion above consumed it).
    analyze_segment(seg, ws, cwd, home, strict)
}

fn looks_like_assignment(tok: &str) -> bool {
    if let Some(eq) = tok.find('=') {
        let head = &tok[..eq];
        !head.is_empty()
            && head
                .chars()
                .next()
                .map(|c| c.is_ascii_alphabetic() || c == '_')
                .unwrap_or(false)
            && head.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    } else {
        false
    }
}

#[allow(clippy::too_many_arguments)]
fn scan_path_token(
    raw: &str,
    write_ctx: bool,
    ws: &Path,
    cwd: &Path,
    home: Option<&Path>,
    strict: bool,
    a: &mut Analysis,
) {
    if raw.is_empty() {
        return;
    }
    let expanded = expand_tilde(raw, home);
    let joined = if expanded.is_absolute() {
        expanded
    } else {
        cwd.join(expanded)
    };
    let norm = normalize(&joined);
    let norm_str = norm.to_string_lossy().to_string();

    if write_ctx && norm_str.contains("/.git/hooks") {
        a.writes_git_hooks = true;
        a.matched_rules.push("git-hooks-write".to_string());
    }

    let under_ws = within(&norm, ws);
    if !under_ws {
        a.paths_outside_workspace = true;
    }

    if let Some(h) = home {
        let hn = normalize(h);
        if within(&norm, &hn) {
            a.touches_home = true;
        }
    }

    if write_ctx && !under_ws {
        a.matched_rules.push("path-escape-write".to_string());
        let escalate = if strict { Verdict::Deny } else { Verdict::Ask };
        if escalate > a.verdict {
            a.verdict = escalate;
        }
    }
}

/// stripHeredocs removes shell here-doc bodies from a command line so the
/// tokenizer never treats the payload as positional arguments (which would
/// fall through to "unknown-command" -> Ask and override an Allow rule such
/// as `cat >> file`). Each `<<[DELIM] ... DELIM` block is replaced by a
/// single space so the surrounding command survives. Here-strings
/// (`<<< word`) and `<<` appearing inside quotes are left untouched.
fn strip_heredocs(cmd: &str) -> String {
    let b: Vec<char> = cmd.chars().collect();
    let mut out = String::with_capacity(cmd.len());
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            '\'' => {
                out.push(c);
                i += 1;
                while i < b.len() && b[i] != '\'' {
                    out.push(b[i]);
                    i += 1;
                }
                if i < b.len() {
                    out.push(b[i]);
                    i += 1;
                }
                continue;
            }
            '"' => {
                out.push(c);
                i += 1;
                while i < b.len() {
                    let ch = b[i];
                    if ch == '\\' && i + 1 < b.len() {
                        out.push(b[i]);
                        out.push(b[i + 1]);
                        i += 2;
                        continue;
                    }
                    out.push(ch);
                    i += 1;
                    if ch == '"' {
                        break;
                    }
                }
                continue;
            }
            _ => {}
        }

        // A here-doc operator is `<<` not followed by `<` (so `<<<` here-strings
        // and `<<` inside the quote regions above stay literal).
        if c == '<' && i + 1 < b.len() && b[i + 1] == '<' && (i + 2 >= b.len() || b[i + 2] != '<') {
            if let Some(after) = consume_heredoc(&b, i) {
                out.push(' ');
                i = after;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// consumeHeredoc, given the index of the `<<`, returns the index just past
/// the closing delimiter line (the body is dropped) if a well-formed
/// here-doc was parsed, else None so the caller falls back to literal text.
fn consume_heredoc(b: &[char], start: usize) -> Option<usize> {
    let mut j = start + 2;
    if j < b.len() && b[j] == '-' {
        j += 1;
    }
    let quote = if j < b.len() && (b[j] == '\'' || b[j] == '"') {
        let q = b[j];
        j += 1;
        Some(q)
    } else {
        None
    };
    let mut del = String::new();
    while j < b.len() {
        let ch = b[j];
        if let Some(q) = quote {
            if ch == q {
                break;
            }
        } else if ch.is_whitespace() {
            break;
        }
        if quote.is_none() && ch == '\\' && j + 1 < b.len() {
            j += 1;
            del.push(b[j]);
            j += 1;
            continue;
        }
        del.push(ch);
        j += 1;
    }
    if del.is_empty() {
        return None;
    }
    // Skip the trailing closing quote if one was opened.
    if quote.is_some() && j < b.len() && b[j] == quote.unwrap() {
        j += 1;
    }
    // Skip the body over lines until a line whose (whitespace-trimmed)
    // content equals the delimiter.
    let mut k = j;
    loop {
        if k >= b.len() {
            break;
        }
        let line_end = b[k..]
            .iter()
            .position(|&ch| ch == '\n')
            .map(|p| k + p)
            .unwrap_or(b.len());
        let mut t = k;
        while t < line_end && (b[t] == ' ' || b[t] == '\t') {
            t += 1;
        }
        let mut content_end = line_end;
        if content_end > t && b[content_end - 1] == '\r' {
            content_end -= 1;
        }
        let content: String = b[t..content_end].iter().collect();
        if content == del {
            k = if line_end < b.len() {
                line_end + 1
            } else {
                line_end
            };
            return Some(k);
        }
        k = if line_end < b.len() {
            line_end + 1
        } else {
            line_end
        };
        if line_end >= b.len() {
            break;
        }
    }
    Some(k)
}

/// Tokenize a shell command into argv segments split at top-level
/// `&&`, `||`, `;`, `|`, `&` and newlines. Single/double quotes and
/// backslash escapes are honored; an unquoted `#` at a word boundary
/// starts a comment running to end of input.
pub fn tokenize(cmd: &str) -> Vec<Vec<String>> {
    let mut segments: Vec<Vec<String>> = Vec::new();
    let mut seg: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut cur_started = false;
    let mut chars = cmd.chars().peekable();

    let flush_tok = |cur: &mut String, started: &mut bool, seg: &mut Vec<String>| {
        if *started {
            seg.push(std::mem::take(cur));
            *started = false;
        }
    };
    let flush_seg = |seg: &mut Vec<String>, segments: &mut Vec<Vec<String>>| {
        if !seg.is_empty() {
            segments.push(std::mem::take(seg));
        }
    };

    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                cur_started = true;
                loop {
                    match chars.next() {
                        Some('\'') | None => break,
                        Some(ch) => cur.push(ch),
                    }
                }
            }
            '"' => {
                cur_started = true;
                loop {
                    match chars.next() {
                        Some('"') | None => break,
                        Some('\\') => {
                            if let Some(next) = chars.next() {
                                cur.push(next);
                            }
                        }
                        Some(ch) => cur.push(ch),
                    }
                }
            }
            '\\' => {
                if let Some(next) = chars.next() {
                    cur.push(next);
                    cur_started = true;
                }
            }
            '#' if !cur_started && cur.is_empty() => {
                let mut newline = false;
                for ch in chars.by_ref() {
                    if ch == '\n' {
                        newline = true;
                        break;
                    }
                }
                flush_tok(&mut cur, &mut cur_started, &mut seg);
                if newline {
                    flush_seg(&mut seg, &mut segments);
                }
            }
            ';' => {
                flush_tok(&mut cur, &mut cur_started, &mut seg);
                flush_seg(&mut seg, &mut segments);
            }
            '\n' | '\r' => {
                flush_tok(&mut cur, &mut cur_started, &mut seg);
                flush_seg(&mut seg, &mut segments);
            }
            '&' | '|' => {
                let doubled = chars.peek() == Some(&c);
                if doubled {
                    chars.next();
                }
                flush_tok(&mut cur, &mut cur_started, &mut seg);
                flush_seg(&mut seg, &mut segments);
            }
            c if c.is_whitespace() => {
                flush_tok(&mut cur, &mut cur_started, &mut seg);
            }
            c => {
                cur.push(c);
                cur_started = true;
            }
        }
    }
    flush_tok(&mut cur, &mut cur_started, &mut seg);
    flush_seg(&mut seg, &mut segments);
    segments
}

/// Key derivation helper mirroring approvals::key_for's command fallback:
/// sha256 hex of the command text.
pub fn command_fallback_key(command: &str) -> String {
    let mut h = Sha256::new();
    h.update(command.as_bytes());
    hex::encode(h.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn v(cmd: &str) -> Verdict {
        analyze(cmd, Path::new("/tmp/ws")).verdict
    }

    // ---------- tokenizer ----------

    #[test]
    fn tokenizer_simple_pipeline() {
        assert_eq!(
            tokenize("echo hi && ls -l | wc -l ; true & rm x"),
            vec![
                vec!["echo", "hi"],
                vec!["ls", "-l"],
                vec!["wc", "-l"],
                vec!["true"],
                vec!["rm", "x"],
            ]
        );
    }

    #[test]
    fn tokenizer_separators_inside_quotes_are_literal() {
        assert_eq!(
            tokenize(r#"echo "a && b; c""#),
            vec![vec!["echo", "a && b; c"]]
        );
        assert_eq!(tokenize("echo 'x | y'"), vec![vec!["echo", "x | y"]]);
        assert_eq!(tokenize("echo 'it''s'"), vec![vec!["echo", "its"]]);
    }

    #[test]
    fn tokenizer_escapes() {
        // Backslash escapes the next character only.
        assert_eq!(tokenize(r"echo a\ b"), vec![vec!["echo", "a b"]]);
        assert_eq!(tokenize(r"echo a\ \&\&\ b"), vec![vec!["echo", "a && b"]]);
        // Unescaped && splits even mid-word, like bash.
        assert_eq!(
            tokenize(r"echo a\ && b"),
            vec![vec!["echo", "a "], vec!["b"]]
        );
        assert_eq!(
            tokenize(r#"echo "\"quoted\"""#),
            vec![vec!["echo", "\"quoted\""]]
        );
        assert_eq!(tokenize("echo '' x"), vec![vec!["echo", "", "x"]]);
    }

    #[test]
    fn tokenizer_comments_stripped_including_separators() {
        assert_eq!(
            tokenize("rm -rf / # cleanup && evil | worse"),
            vec![vec!["rm", "-rf", "/"]]
        );
        assert_eq!(tokenize("echo hi#tag"), vec![vec!["echo", "hi#tag"]]);
        assert_eq!(
            tokenize("true # one\necho two"),
            vec![vec!["true"], vec!["echo", "two"]]
        );
    }

    #[test]
    fn tokenizer_adjacent_and_trailing_separators() {
        assert_eq!(
            tokenize("echo hi&&ls"),
            vec![vec!["echo", "hi"], vec!["ls"]]
        );
        assert_eq!(tokenize("true &&"), vec![vec!["true"]]);
        assert_eq!(tokenize(""), Vec::<Vec<String>>::new());
        assert_eq!(tokenize("   "), Vec::<Vec<String>>::new());
        assert_eq!(tokenize("|| x"), vec![vec!["x"]]);
    }

    #[test]
    fn tokenizer_newline_is_a_separator() {
        assert_eq!(tokenize("ls\npwd"), vec![vec!["ls"], vec!["pwd"]]);
    }

    // ---------- rules ----------

    #[test]
    fn table_has_at_least_50_rules() {
        assert!(rule_count() >= 50, "rule table has {}", rule_count());
    }

    #[test]
    fn safe_readonly_commands_allow() {
        for cmd in [
            "ls -la",
            "cat foo.txt",
            "rg pattern src/",
            "git status --porcelain",
            "git log --oneline -5",
            "head -n 20 main.go",
            "which cargo",
            "/usr/bin/file image.png",
            "jq .keys config.json",
            "find . -name '*.go'",
        ] {
            assert_eq!(v(cmd), Verdict::Allow, "{cmd} should be Allow");
        }
    }

    #[test]
    fn destructive_commands_deny() {
        for cmd in [
            "rm -rf /",
            "rm -rf ~",
            "sudo make install",
            "su - root",
            "shutdown -h now",
            "mkfs.ext4 /dev/sda1",
            "dd if=zero of=/dev/rdisk0",
            ":(){ :|:& };:",
            "launchctl unload ~/Library/LaunchAgents/x.plist",
            "csrutil disable",
            "diskutil eraseDisk APFS Test disk1",
            "chmod 777 /usr/bin",
        ] {
            assert_eq!(v(cmd), Verdict::Deny, "{cmd} should be Deny");
        }
    }

    #[test]
    fn network_tools_ask() {
        for cmd in [
            "curl http://example.com",
            "wget https://example.com/f.tar.gz",
            "ssh host",
            "git push origin main",
            "pip install requests",
            "npm publish",
            "cargo install ripgrep",
            "brew install jq",
            "nc -l 8080",
        ] {
            let a = analyze(cmd, Path::new("/tmp/ws"));
            assert_eq!(a.verdict, Verdict::Ask, "{cmd} should be Ask");
            assert!(a.uses_network, "{cmd} should set uses_network");
        }
    }

    #[test]
    fn cargo_workspace_build_test_allow() {
        for cmd in [
            "cargo test -p atom-tui",
            "cargo build",
            "cargo check",
            "cargo clippy",
            "cargo doc",
            "cargo fmt",
            "cargo clean",
            "cargo run --bin atom",
        ] {
            assert_eq!(v(cmd), Verdict::Allow, "{cmd} should be Allow");
        }
        // install / publish / add remain Ask (network/package manager)
        for cmd in ["cargo install ripgrep", "cargo publish", "cargo add serde"] {
            assert_eq!(v(cmd), Verdict::Ask, "{cmd} should be Ask");
        }
    }

    #[test]
    fn dd_arity_constraints() {
        // dd with of=/dev/* denies...
        assert_eq!(v("dd if=/dev/zero of=/dev/rdisk0"), Verdict::Deny);
        // ...but plain dd (no device target) only asks.
        assert_eq!(v("dd if=a of=b bs=1m"), Verdict::Ask);
        assert_eq!(v("dd"), Verdict::Ask);
    }

    #[test]
    fn chmod_requires_mode_and_path() {
        assert_eq!(v("chmod +x build.sh"), Verdict::Allow);
        assert_eq!(v("chmod 755 /tmp/a /tmp/b"), Verdict::Allow);
        // Missing operands falls through the arity constraint to unknown->Ask.
        assert_eq!(v("chmod +x"), Verdict::Ask);
    }

    #[test]
    fn precedence_deny_beats_ask_beats_allow_across_segments() {
        assert_eq!(v("ls && curl example.com"), Verdict::Ask);
        assert_eq!(v("ls && sudo rm x"), Verdict::Deny);
        assert_eq!(v("curl x; cat y"), Verdict::Ask);
        assert_eq!(v("cat y; ls -l | wc"), Verdict::Allow);
    }

    #[test]
    fn unknown_command_defaults_to_ask() {
        assert_eq!(v("./mytool --flag"), Verdict::Ask);
        assert_eq!(v("totally-unknown-binary arg"), Verdict::Ask);
        let a = analyze("totally-unknown", Path::new("/tmp/ws"));
        assert!(a.matched_rules.contains(&"unknown-command".to_string()));
    }

    #[test]
    fn heredoc_payload_is_not_treated_as_args() {
        // The orchestrator's append flow: the heredoc body must not become
        // positional tokens (which would fall through to unknown-command ->
        // Ask and override the read-file Allow).
        assert_eq!(
            v("cat >> haha.txt <<'EOF'\nYOUR LINE HERE\nEOF"),
            Verdict::Allow
        );
        assert_eq!(v("cat >> haha.txt <<EOF\nhello world\nEOF"), Verdict::Allow);
        // Multiple heredocs still analyze the surrounding command.
        assert_eq!(
            v("cat a.txt <<'A'\nx\nA\ncat b.txt <<'B'\ny\nB"),
            Verdict::Allow
        );
        // Here-strings (`<<<`) are not heredocs: left verbatim.
        assert_eq!(strip_heredocs("command <<< word"), "command <<< word");
        // `<<` inside quotes is literal text, not a heredoc start.
        assert_eq!(strip_heredocs("cmd 'a<<b'"), "cmd 'a<<b'");
        assert_eq!(strip_heredocs("cmd \"x<<y\""), "cmd \"x<<y\"");
    }

    #[test]
    fn empty_command_is_vacuously_allowed() {
        let a = analyze("", Path::new("/tmp/ws"));
        assert_eq!(a.verdict, Verdict::Allow);
        assert!(a.segments.is_empty());
    }

    // ---------- path scan ----------

    #[test]
    fn path_scan_flags_writes_outside_workspace() {
        let ws = Path::new("/Users/dev/proj");
        let a = analyze("touch ../outside.txt", ws);
        assert!(a.paths_outside_workspace);
        assert_eq!(a.verdict, Verdict::Ask);

        let b = analyze("cat /etc/hosts", ws);
        assert!(b.paths_outside_workspace);
        assert_eq!(b.verdict, Verdict::Allow); // read-only escape: info only

        let c = analyze("touch sub/in-ws.txt", ws);
        assert!(!c.paths_outside_workspace);
        assert_eq!(c.verdict, Verdict::Allow);

        let d = analyze("rm -rf ../outside", ws);
        assert!(d.paths_outside_workspace);
        assert_eq!(d.verdict, Verdict::Ask);

        let e = analyze("rm -rf sub/in-ws", ws);
        assert!(!e.paths_outside_workspace);
        assert_eq!(e.verdict, Verdict::Allow);
    }

    #[test]
    fn strict_mode_denies_write_escapes() {
        let a = analyze_full(
            "touch /tmp/x",
            Path::new("/Users/dev/proj"),
            Path::new("/Users/dev/proj"),
            true,
        );
        assert_eq!(a.verdict, Verdict::Deny);
    }

    #[test]
    fn path_scan_detects_dotdot_escape_via_cwd_resolution() {
        let a = analyze_full(
            "echo x > ../../../escape.txt",
            Path::new("/home/dev/proj"),
            Path::new("/home/dev/proj/sub/dir"),
            false,
        );
        assert!(a.paths_outside_workspace);
        assert_eq!(a.verdict, Verdict::Ask);
    }

    #[test]
    fn git_hooks_write_flagged_and_denied() {
        let a = analyze("cp evil.sh .git/hooks/pre-commit", Path::new("/ws"));
        assert!(a.writes_git_hooks);
        assert_eq!(a.verdict, Verdict::Deny);
        assert!(a.matched_rules.contains(&"git-hooks-write".to_string()));

        // Reading hook listings stays allowed.
        let b = analyze("ls .git/hooks/", Path::new("/ws"));
        assert!(!b.writes_git_hooks);
    }

    #[test]
    fn tilde_paths_touch_home() {
        let a = analyze("cat ~/.zshrc", Path::new("/Users/dev/proj"));
        assert!(a.touches_home);

        let b = analyze("touch ~/outside-probe", Path::new("/Users/dev/proj"));
        assert!(b.touches_home);
        assert_eq!(b.verdict, Verdict::Ask);
    }

    #[test]
    fn urls_do_not_count_as_paths() {
        let a = analyze("git clone https://github.com/x/y.git", Path::new("/ws"));
        // clone asks as a network op, not as a path escape
        assert_eq!(a.verdict, Verdict::Ask);
        assert!(a.uses_network);
    }

    #[test]
    fn leading_assignments_stripped_for_matching() {
        assert_eq!(v("FOO=bar ls -l"), Verdict::Allow);
        assert_eq!(v("GOGC=off GOFLAGS=-mod=mod go version"), Verdict::Ask); // go unmatched
    }

    // ---------- nested shells ----------

    #[test]
    fn nested_shell_payload_recursed() {
        assert_eq!(v("bash -c 'ls -la'"), Verdict::Allow);
        assert_eq!(v("sh -c \"sudo reboot\""), Verdict::Deny);
        assert_eq!(v("zsh -lc 'curl example.com'"), Verdict::Ask);
    }

    #[test]
    fn interactive_shell_falls_back_to_ask() {
        assert_eq!(v("bash"), Verdict::Ask);
        assert_eq!(v("deploy.sh"), Verdict::Ask);
    }

    // ---------- misc helpers ----------

    #[test]
    fn fallback_key_is_sha256_hex() {
        assert_eq!(
            command_fallback_key("abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn verdict_ordering_matches_precedence() {
        assert!(Verdict::Allow < Verdict::Ask);
        assert!(Verdict::Ask < Verdict::Deny);
    }

    #[test]
    fn temp_workspace_analysis_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "hi").unwrap();
        let a = analyze("cat a.txt && touch b.txt && rm b.txt", dir.path());
        assert_eq!(a.verdict, Verdict::Allow);
        assert_eq!(a.segments.len(), 3);
    }
}
