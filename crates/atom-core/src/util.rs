//! Small shared helpers ported from main.go / session.go utilities.

use sha2::{Digest, Sha256};

/// firstLineTrunc returns the first line of s, truncated to max runes
/// with an ellipsis.
pub fn first_line_trunc(s: &str, max: usize) -> String {
    let first = s.lines().next().unwrap_or("");
    if first.chars().count() <= max {
        return first.to_string();
    }
    let mut out: String = first.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// shell_split tokenizes a single command line the way POSIX shells
/// do for unquoted / single-quoted / double-quoted tokens. It's
/// intentionally tiny — only quoting rules matter here, since
/// `$EDITOR` values like `code --wait` or `'emacs -nw'` would
/// otherwise be passed to `Command::new` as one malformed argv.
pub(crate) fn shell_split(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut started = false;
    for ch in s.chars() {
        match ch {
            '\'' if !in_double => {
                in_single = !in_single;
                started = true;
            }
            '"' if !in_single => {
                in_double = !in_double;
                started = true;
            }
            c if c.is_whitespace() && !in_single && !in_double => {
                if started {
                    out.push(std::mem::take(&mut cur));
                    started = false;
                }
            }
            c => {
                cur.push(c);
                started = true;
            }
        }
    }
    if started {
        out.push(cur);
    }
    out
}

/// is_terminal_editor returns true for command names known to run
/// inside the current TTY (vim, nvim, emacs, nano, …). Launching
/// one from atom's click handler would hijack stdin/stdout and
/// freeze the TUI, so we fall through to the OS opener instead.
///
/// `program` may be a bare executable or a quoted command line —
/// `"code --wait"` and `"emacsclient -c"` both pass through. The
/// check operates on the first token's basename.
pub(crate) fn is_terminal_editor(program: &str) -> bool {
    let first = shell_split(program)
        .into_iter()
        .next()
        .unwrap_or_else(|| program.to_string());
    let basename = std::path::Path::new(&first)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(first.as_str());
    matches!(
        basename,
        "vim"
            | "nvim"
            | "vi"
            | "ex"
            | "view"
            | "rgvim"
            | "nano"
            | "pico"
            | "jed"
            | "emacs"
            | "emacsclient"
            | "mg"
            | "ne"
            | "joe"
            | "mcedit"
            | "ed"
    )
}

/// preferred_editor returns the user's shell-config editor command:
/// `$VISUAL` first, then `$EDITOR`, both inherited from the parent
/// shell (so edits in `.zshrc` land here after a `source` or new
/// terminal pane). Empty / whitespace-only values are treated as
/// unset.
pub(crate) fn preferred_editor() -> Option<String> {
    let vis = std::env::var("VISUAL")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if let Some(v) = vis {
        return Some(v);
    }
    std::env::var("EDITOR")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// percent_decode reverses `escaped_path`: %XX back to the byte.
/// Only `%` followed by two hex digits is decoded; everything else
/// is left intact so paths that legitimately contain `%` survive.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_else(|_| s.to_string())
}

/// editor_invocation returns the argv (program + args) to spawn for
/// a `file://` URI: the file path with any `?line=N` query and
/// percent-encoding stripped. Returns `None` when the user has no
/// `$VISUAL`/`$EDITOR`, or when the configured editor is a known
/// terminal editor that would steal atom's TTY.
pub(crate) fn editor_invocation(url: &str) -> Option<Vec<String>> {
    let raw = url.strip_prefix("file://")?;
    // Strip ?line=N[ -M|,M] — most GUI editors don't understand
    // query strings, and the Go path simply dropped these too.
    let path_part = raw.split('?').next().unwrap_or(raw);
    let path = percent_decode(path_part);
    let cmd = preferred_editor()?;
    if is_terminal_editor(&cmd) {
        return None;
    }
    let mut argv = shell_split(&cmd);
    if argv.is_empty() {
        argv.push(cmd);
    }
    argv.push(path);
    Some(argv)
}

/// openURL opens a URL with the platform opener (`open` on macOS,
/// `xdg-open` on Linux); used for clicked OSC 8 hyperlinks and any
/// other user-facing URL. Spawn failures are ignored: the URL is
/// already visible in the transcript.
///
/// For `file://` URIs, `$VISUAL` (then `$EDITOR`) takes precedence
/// so a `source ~/.zshrc`'d `export EDITOR=code` makes path clicks
/// land in VS Code instead of whatever the OS picked. Terminal
/// editors are skipped here — see `is_terminal_editor`.
pub fn open_url(url: &str) {
    if url.starts_with("file://") {
        if let Some(argv) = editor_invocation(url) {
            if let Some((program, args)) = argv.split_first() {
                if std::process::Command::new(program)
                    .args(args)
                    .spawn()
                    .is_ok()
                {
                    return;
                }
            }
        }
    }
    let result = if cfg!(target_os = "macos") {
        std::process::Command::new("open").arg(url).spawn()
    } else if cfg!(target_os = "linux") {
        std::process::Command::new("xdg-open").arg(url).spawn()
    } else {
        return;
    };
    let _ = result;
}

/// formatTokens renders a token count compactly: 7600 -> "7.6K".
pub fn format_tokens(n: i64) -> String {
    if n >= 1_000_000 {
        format_trim(n as f64 / 1_000_000.0, "M")
    } else if n >= 1_000 {
        format_trim(n as f64 / 1_000.0, "K")
    } else {
        n.to_string()
    }
}

fn format_trim(v: f64, suffix: &str) -> String {
    let s = format!("{:.1}", v);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    format!("{}{}", s, suffix)
}

/// sha256Hash returns the lowercase hex SHA-256 of data.
pub fn sha256_hash(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex::encode(h.finalize())
}

/// addStreamUsage folds src into dst field-wise (Go session.go).
pub fn add_stream_usage(dst: &mut crate::types::StreamUsage, src: &crate::types::StreamUsage) {
    dst.prompt_tokens += src.prompt_tokens;
    dst.completion_tokens += src.completion_tokens;
    dst.total_tokens += src.total_tokens;
    dst.reasoning_tokens += src.reasoning_tokens;
    dst.cache_read_tokens += src.cache_read_tokens;
    dst.cache_write_tokens += src.cache_write_tokens;
    dst.cost += src.cost;
    dst.prompt_tokens_all += src.prompt_tokens;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncates_first_line() {
        assert_eq!(first_line_trunc("hello\nworld", 10), "hello");
        assert_eq!(first_line_trunc("abcdefghijk", 6), "abcde…");
    }

    #[test]
    fn formats_tokens() {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(999), "999");
        assert_eq!(format_tokens(1000), "1K");
        assert_eq!(format_tokens(7600), "7.6K");
        assert_eq!(format_tokens(2_500_000), "2.5M");
    }

    #[test]
    fn hashes() {
        assert_eq!(
            sha256_hash(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    /// helper: take a static pair out of the environment for the
    /// duration of one test, then restore it on drop. The Mutex
    /// serializes env-touching tests so they don't race each other
    /// (env vars are process-global).
    struct EnvGuard {
        key: &'static str,
        prev: Option<String>,
    }
    impl EnvGuard {
        fn set(key: &'static str, value: &str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::set_var(key, value);
            Self { key, prev }
        }
        fn unset(key: &'static str) -> Self {
            let prev = std::env::var(key).ok();
            std::env::remove_var(key);
            Self { key, prev }
        }
    }
    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var(self.key, v),
                None => std::env::remove_var(self.key),
            }
        }
    }

    /// Serialize env-var tests so two of them can't race each other
    /// while $VISUAL/$EDITOR are mid-flight. Acquire before setting.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn shell_split_handles_unquoted_and_quoted() {
        assert_eq!(shell_split("code --wait"), vec!["code", "--wait"]);
        assert_eq!(shell_split("'emacs -nw'"), vec!["emacs -nw"]);
        assert_eq!(
            shell_split("mate -l 42 foo"),
            vec!["mate", "-l", "42", "foo"]
        );
        // Empty / whitespace-only input yields nothing.
        assert!(shell_split("").is_empty());
        assert!(shell_split("   ").is_empty());
    }

    #[test]
    fn terminal_editor_recognized() {
        for cmd in ["vim", "nvim", "/usr/bin/nvim", "emacsclient -c"] {
            assert!(is_terminal_editor(cmd), "{cmd} should be flagged");
        }
        for cmd in [
            "code",
            "code --wait",
            "/usr/local/bin/cursor",
            "subl",
            "mate",
        ] {
            assert!(!is_terminal_editor(cmd), "{cmd} should NOT be flagged");
        }
    }

    #[test]
    fn visual_takes_precedence_over_editor() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _v = EnvGuard::set("VISUAL", "code --wait");
        let _e = EnvGuard::set("EDITOR", "vim");
        assert_eq!(preferred_editor().as_deref(), Some("code --wait"));
    }

    #[test]
    fn falls_back_to_editor_when_visual_missing() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _v = EnvGuard::unset("VISUAL");
        let _e = EnvGuard::set("EDITOR", "nvim");
        assert_eq!(preferred_editor().as_deref(), Some("nvim"));
    }

    #[test]
    fn empty_or_whitespace_visual_treated_as_unset() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _v = EnvGuard::set("VISUAL", "   ");
        let _e = EnvGuard::set("EDITOR", "code");
        assert_eq!(preferred_editor().as_deref(), Some("code"));
    }

    #[test]
    fn no_editor_means_no_invocation() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _v = EnvGuard::unset("VISUAL");
        let _e = EnvGuard::unset("EDITOR");
        assert!(editor_invocation("file:///tmp/foo.rs").is_none());
    }

    #[test]
    fn terminal_editor_invocation_is_none() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _v = EnvGuard::unset("VISUAL");
        let _e = EnvGuard::set("EDITOR", "vim");
        // Terminal editors would steal atom's TTY, so we fall through
        // to the OS opener rather than spawn them.
        assert!(editor_invocation("file:///tmp/foo.rs").is_none());
    }

    #[test]
    fn gui_editor_invocation_drops_query_and_decodes_path() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _v = EnvGuard::unset("VISUAL");
        let _e = EnvGuard::set("EDITOR", "code --reuse-window");
        let argv = editor_invocation("file:///Users/me/My%20Docs/foo.rs?line=42")
            .expect("EDITOR=code should produce an argv");
        assert_eq!(
            argv,
            vec!["code", "--reuse-window", "/Users/me/My Docs/foo.rs"]
        );
    }

    #[test]
    fn gui_editor_invocation_with_bare_path() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _v = EnvGuard::set("VISUAL", "cursor");
        let _e = EnvGuard::unset("EDITOR");
        let argv =
            editor_invocation("file:///tmp/foo.rs").expect("VISUAL=cursor should produce an argv");
        assert_eq!(argv, vec!["cursor", "/tmp/foo.rs"]);
    }

    #[test]
    fn non_file_url_has_no_invocation() {
        let _lock = ENV_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let _v = EnvGuard::set("VISUAL", "code");
        let _e = EnvGuard::unset("EDITOR");
        // http(s) URIs bypass the editor branch entirely — the
        // browser opener (`open` / `xdg-open`) is the right path
        // there.
        assert!(editor_invocation("https://example.com").is_none());
        assert!(editor_invocation("file:///tmp/x").is_some());
    }
}
