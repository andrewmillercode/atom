//! write_file / edit_file, ported from file_edit.go: per-session
//! file-seen bookkeeping, drift re-checks returning compact diffs, atomic
//! writes, cross-process advisory locks, and model-facing error strings.
//!
//! Port addition: every write is gated on path containment within
//! ctx.cwd; outside-cwd writes consult the approver with rule id
//! "fs_write_outside" and proceed only on allow decisions.

use crate::{ToolCtx, ToolOutcome};
use atom_core::render::diff::file_diff;
use atom_core::util::sha256_hash;
use atom_sandbox::approvals::ApprovalRequest;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub(crate) const MAX_DRIFT_DIFF_LINES: usize = 80;
pub(crate) const FILE_LOCK_WAIT: Duration = Duration::from_secs(10);
pub(crate) const FILE_LOCK_POLL: Duration = Duration::from_millis(20);
/// Maximum number of file observations retained by one session.
pub const MAX_FILE_SEEN_ENTRIES: usize = 128;
/// Maximum total bytes of prior file contents retained by one session.
pub const MAX_FILE_SEEN_BYTES: usize = 16 * 1024 * 1024;
const NEARBY_CONTEXT_RADIUS: usize = 2;
const MAX_DUPLICATE_HITS: usize = 5;

// ---------------------------------------------------------------------------
// Per-session seen-file cache (Go keeps this on Session).
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct FileSeenEntry {
    pub hash: String,
    pub data: Option<Vec<u8>>,
}

#[derive(Default)]
struct FileSeenCache {
    entries: HashMap<String, FileSeenEntry>,
    lru: VecDeque<String>,
    retained_bytes: usize,
}

impl FileSeenCache {
    fn remove(&mut self, key: &str) -> Option<FileSeenEntry> {
        let entry = self.entries.remove(key)?;
        self.retained_bytes -= entry.data.as_ref().map_or(0, Vec::len);
        if let Some(pos) = self.lru.iter().position(|candidate| candidate == key) {
            self.lru.remove(pos);
        }
        Some(entry)
    }

    fn touch(&mut self, key: &str) {
        if let Some(pos) = self.lru.iter().position(|candidate| candidate == key) {
            self.lru.remove(pos);
        }
        self.lru.push_back(key.to_string());
    }

    fn evict_to_limits(&mut self) {
        while self.entries.len() > MAX_FILE_SEEN_ENTRIES
            || self.retained_bytes > MAX_FILE_SEEN_BYTES
        {
            let Some(key) = self.lru.pop_front() else {
                break;
            };
            if let Some(entry) = self.entries.remove(&key) {
                self.retained_bytes -= entry.data.as_ref().map_or(0, Vec::len);
            }
        }
    }
}

/// Bounded fileSeen LRU for one session, keyed by canonical path.
#[derive(Default)]
pub struct FileSeen(Mutex<FileSeenCache>);

impl FileSeen {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn remember(&self, path: &Path, data: &[u8]) {
        let key = canonical_file_path(path);
        let entry = FileSeenEntry {
            hash: sha256_hash(data),
            data: (data.len() <= MAX_FILE_SEEN_BYTES).then(|| data.to_vec()),
        };
        if let Ok(mut cache) = self.0.lock() {
            cache.remove(&key);
            cache.retained_bytes += entry.data.as_ref().map_or(0, Vec::len);
            cache.entries.insert(key.clone(), entry);
            cache.touch(&key);
            cache.evict_to_limits();
        }
    }

    pub fn seen(&self, path: &Path) -> Option<FileSeenEntry> {
        let key = canonical_file_path(path);
        let mut cache = self.0.lock().ok()?;
        let entry = cache.entries.get(&key)?.clone();
        cache.touch(&key);
        Some(entry)
    }

    #[cfg(test)]
    fn cache_usage(&self) -> (usize, usize) {
        self.0
            .lock()
            .map(|cache| (cache.entries.len(), cache.retained_bytes))
            .unwrap_or_default()
    }
}

/// rememberFile on the ctx's session cache; a missing cache mirrors Go's
/// nil-Session no-op.
pub(crate) fn remember_file(ctx: &ToolCtx<'_>, path: &Path, data: &[u8]) {
    if let Some(fs) = ctx.file_seen {
        fs.remember(path, data);
    }
}

fn seen_file(ctx: &ToolCtx<'_>, path: &Path) -> Option<FileSeenEntry> {
    ctx.file_seen.and_then(|fs| fs.seen(path))
}

/// canonicalFilePath resolves symlinks; falls back to the absolute path.
pub(crate) fn canonical_file_path(path: &Path) -> String {
    match std::fs::canonicalize(path) {
        Ok(p) => p.to_string_lossy().to_string(),
        Err(_) => std::path::absolute(path)
            .unwrap_or_else(|_| path.to_path_buf())
            .to_string_lossy()
            .to_string(),
    }
}

// ---------------------------------------------------------------------------
// Error strings and diff helpers.
// ---------------------------------------------------------------------------

pub(crate) fn not_read_error(path: &str) -> String {
    format!(
        "error: file has not been read in this session. Call read_file on {path} first (offset/limit is enough)."
    )
}

pub(crate) fn file_busy_error() -> String {
    "error: file is being edited by another atom process.".to_string()
}

pub(crate) fn file_changed_error(path: &str, old: &[u8], neu: &[u8]) -> String {
    let diff = file_diff(path, old, neu);
    let (capped, truncated) = cap_diff_lines(&diff, MAX_DRIFT_DIFF_LINES);
    let mut b = String::new();
    b.push_str(
        "error: file changed since last observation. Edit was not applied. Retry using this update:\n\n",
    );
    b.push_str(&capped);
    if truncated {
        let offset = first_changed_line_offset(&diff);
        b.push_str(&format!(
            "\n[diff truncated after {} lines]\nCall read_file path={} offset={} limit=80 to see more.",
            MAX_DRIFT_DIFF_LINES, path, offset
        ));
    }
    b
}

fn file_changed_without_snapshot_error(path: &str) -> String {
    format!(
        "error: file changed since last observation. Edit was not applied. The previous contents exceeded the seen-file snapshot limit, so no diff is available. Call read_file on {path} to inspect the current contents, then retry."
    )
}

pub(crate) fn cap_diff_lines(diff: &str, max_lines: usize) -> (String, bool) {
    if diff.is_empty() {
        return (diff.to_string(), false);
    }
    let mut lines: Vec<&str> = diff.split('\n').collect();
    // Split keeps a trailing empty element when diff ends with newline.
    let n = lines.len();
    if n > 0 && lines[n - 1].is_empty() {
        lines.truncate(n - 1);
    }
    if lines.len() <= max_lines {
        return (diff.to_string(), false);
    }
    (lines[..max_lines].join("\n"), true)
}

pub(crate) fn first_changed_line_offset(diff: &str) -> i64 {
    for line in diff.split('\n') {
        if let Some(rest) = line.strip_prefix("@@ -") {
            let num: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            if let Ok(old_start) = num.parse::<i64>() {
                if old_start > 0 {
                    return old_start - 1;
                }
            }
        }
    }
    0
}

pub(crate) fn count_file_lines(content: &str) -> usize {
    if content.is_empty() {
        return 0;
    }
    let n = content.matches('\n').count();
    if content.ends_with('\n') {
        n
    } else {
        n + 1
    }
}

pub(crate) fn read_file_output(content: &str, offset: i64, limit: i64) -> String {
    let limit = if limit <= 0 {
        atom_core::render::diff::DEFAULT_READ_FILE_LIMIT
    } else {
        limit
    };
    let offset = offset.max(0);
    let mut window = atom_core::render::diff::file_line_window(content, offset, limit);
    let total = count_file_lines(content) as i64;
    let mut shown = limit;
    if offset + shown > total {
        shown = total - offset;
    }
    if shown < 0 {
        shown = 0;
    }
    if offset + shown < total {
        let remaining = total - offset - shown;
        let next = offset + shown;
        if !window.is_empty() && !window.ends_with('\n') {
            window.push('\n');
        }
        window.push_str(&format!(
            "[{remaining} more lines. Use offset={next} to continue.]"
        ));
    }
    window
}

fn file_lines(content: &str) -> Vec<&str> {
    if content.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<&str> = content.split('\n').collect();
    if content.ends_with('\n') && !lines.is_empty() {
        lines.pop();
    }
    lines
}

fn format_numbered(lines: &[&str], start: isize, end: isize) -> String {
    let start = start.max(0) as usize;
    let end = (end as usize).min(lines.len());
    if start >= end {
        return String::new();
    }
    let mut width = format!("{}", end).len();
    if width < 6 {
        width = 6;
    }
    let mut b = String::new();
    for (i, line) in lines.iter().enumerate().skip(start).take(end - start) {
        b.push_str(&format!("{:>width$}|{}\n", i + 1, line, width = width));
    }
    b.trim_end_matches('\n').to_string()
}

fn line_index_at(content: &str, byte_offset: usize) -> usize {
    let off = byte_offset.min(content.len());
    content[..off].matches('\n').count()
}

fn first_non_empty_line(s: &str) -> &str {
    s.split('\n').find(|l| !l.trim().is_empty()).unwrap_or("")
}

fn old_text_not_found_error(content: &str, old_text: &str) -> String {
    let lines = file_lines(content);
    let mut idx = 0usize;
    let needle = first_non_empty_line(old_text);
    if !needle.is_empty() {
        for (i, line) in lines.iter().enumerate() {
            if line.contains(needle) {
                idx = i;
                break;
            }
        }
    }
    let start = idx.saturating_sub(NEARBY_CONTEXT_RADIUS);
    let mut end = idx + NEARBY_CONTEXT_RADIUS + 4;
    if end > lines.len() {
        end = lines.len();
    }
    if end <= start && !lines.is_empty() {
        end = lines.len().min(start + 6);
    }
    format!(
        "error: old_text not found. Nearby (lines {}-{}):\n\n{}",
        start + 1,
        end,
        format_numbered(&lines, start as isize, end as isize)
    )
}

fn old_text_duplicate_error(content: &str, old_text: &str, count: usize) -> String {
    let lines = file_lines(content);
    let mut b = format!("error: old_text found {count} times. Include more context.\n");
    let mut shown = 0;
    let mut search_from = 0usize;
    while shown < MAX_DUPLICATE_HITS {
        let rel = content[search_from.min(content.len())..].find(old_text);
        let Some(rel) = rel else { break };
        let abs = search_from + rel;
        let line = line_index_at(content, abs);
        let start = line.saturating_sub(1);
        let mut end = line + 2;
        if end > lines.len() {
            end = lines.len();
        }
        b.push_str(&format!(
            "\nline {}:\n{}\n",
            line + 1,
            format_numbered(&lines, start as isize, end as isize)
        ));
        search_from = abs + old_text.len();
        shown += 1;
    }
    b.trim_end_matches('\n').to_string()
}

// ---------------------------------------------------------------------------
// Atomic writes and advisory locking.
// ---------------------------------------------------------------------------

fn write_file_atomic(path: &str, data: &[u8]) -> Result<(), std::io::Error> {
    use std::io::Write;
    let p = Path::new(path);
    let dir = p.parent().unwrap_or(Path::new("."));
    let perm = std::fs::metadata(p)
        .map(|m| m.permissions())
        .unwrap_or_else(|_| {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::Permissions::from_mode(0o644)
            }
            #[cfg(not(unix))]
            {
                std::fs::Permissions::default()
            }
        });
    let mut tmp = tempfile::Builder::new()
        .prefix(".atom-tmp-")
        .rand_bytes(6)
        .tempfile_in(dir)?;
    tmp.as_file().set_permissions(perm)?;
    tmp.write_all(data)?;
    tmp.as_file().sync_all()?;
    let res = tmp.persist(p);
    match res {
        Ok(_) => Ok(()),
        Err(e) => Err(e.error),
    }
}

fn lock_file_path_in(locks_dir: &Path, target: &str) -> PathBuf {
    let key = canonical_file_path(Path::new(target));
    locks_dir.join(sha256_hash(key.as_bytes()))
}

fn default_locks_dir() -> PathBuf {
    atom_core::session::store::data_dir().join("locks")
}

/// lockFilePath against the process data dir.
pub fn lock_file_path(target: &str) -> PathBuf {
    lock_file_path_in(&default_locks_dir(), target)
}

struct FlockGuard<'a>(&'a std::fs::File);

impl Drop for FlockGuard<'_> {
    fn drop(&mut self) {
        unsafe {
            libc::flock(std::os::unix::io::AsRawFd::as_raw_fd(self.0), libc::LOCK_UN);
        }
    }
}

/// withFileLock acquires LOCK_EX on the canonical lock file, polling
/// every 20ms until timeout, then reports fileBusyError.
pub(crate) fn with_file_lock_timeout<R>(
    locks_dir: &Path,
    target: &str,
    timeout: Duration,
    f: impl FnOnce() -> R,
) -> Result<R, String> {
    let dir = locks_dir.to_path_buf();
    std::fs::create_dir_all(&dir).map_err(|e| format!("error locking file: {e}"))?;
    let path = lock_file_path_in(&dir, target);
    let f_handle = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(&path)
        .map_err(|e| format!("error locking file: {e}"))?;
    let deadline = Instant::now() + timeout;
    loop {
        let rc = unsafe {
            libc::flock(
                std::os::unix::io::AsRawFd::as_raw_fd(&f_handle),
                libc::LOCK_EX | libc::LOCK_NB,
            )
        };
        if rc == 0 {
            let _guard = FlockGuard(&f_handle);
            return Ok(f());
        }
        if Instant::now() > deadline {
            return Err(file_busy_error());
        }
        std::thread::sleep(FILE_LOCK_POLL);
    }
}

fn with_file_lock<R>(target: &str, f: impl FnOnce() -> R) -> Result<R, String> {
    with_file_lock_timeout(&default_locks_dir(), target, FILE_LOCK_WAIT, f)
}

// ---------------------------------------------------------------------------
// Drift check + tool bodies.
// ---------------------------------------------------------------------------

/// observeOrDrift verifies the on-disk content matches what the session
/// last observed. On drift it refreshes the last-seen snapshot and
/// returns the compact-diff error.
fn observe_or_drift(ctx: &ToolCtx<'_>, path: &Path, disk: &[u8]) -> Result<(), String> {
    let display = path.display().to_string();
    match seen_file(ctx, path) {
        None => Err(not_read_error(&display)),
        Some(seen) => {
            if seen.hash != sha256_hash(disk) {
                remember_file(ctx, path, disk);
                return Err(match seen.data {
                    Some(data) => file_changed_error(&display, &data, disk),
                    None => file_changed_without_snapshot_error(&display),
                });
            }
            Ok(())
        }
    }
}

/// Gate helper shared by write_file/edit_file: paths inside ctx.cwd pass
/// untouched; anything else asks the approver (rule "fs_write_outside").
pub(crate) async fn gate_fs_write(ctx: &ToolCtx<'_>, abs: &Path) -> Result<(), String> {
    let cwd_norm = canonicalize_with_missing(&ctx.cwd);
    let target_norm = canonicalize_with_missing(abs);
    if target_norm.starts_with(&cwd_norm) {
        return Ok(());
    }
    let decision = ctx
        .approver
        .decide(ApprovalRequest {
            session_id: ctx.session_id.clone(),
            command: abs.display().to_string(),
            cwd: ctx.cwd.clone(),
            rule_id: "fs_write_outside".to_string(),
            reason: "writes outside the workspace directory".to_string(),
        })
        .await;
    if decision.allows() {
        Ok(())
    } else {
        Err("error: write outside the workspace was not approved".to_string())
    }
}

/// Canonicalize the nearest existing ancestor, then restore any missing
/// suffix. This catches workspace symlinks that point outside the cwd while
/// still supporting creation of new nested paths.
fn canonicalize_with_missing(path: &Path) -> PathBuf {
    let path = normalize(path);
    let mut ancestor = path.as_path();
    let mut suffix = Vec::new();
    while !ancestor.exists() {
        let Some(name) = ancestor.file_name() else {
            break;
        };
        suffix.push(name.to_os_string());
        let Some(parent) = ancestor.parent() else {
            break;
        };
        ancestor = parent;
    }
    let mut out = std::fs::canonicalize(ancestor).unwrap_or_else(|_| ancestor.to_path_buf());
    for component in suffix.into_iter().rev() {
        out.push(component);
    }
    out
}

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

fn parse_err(e: serde_json::Error) -> ToolOutcome {
    ToolOutcome {
        text: format!("error parsing arguments: {e}"),
        ..Default::default()
    }
}

fn outcome(text: String, diff: String) -> ToolOutcome {
    ToolOutcome {
        text: text_with_diff(text, &diff),
        images: Vec::new(),
        diff,
    }
}

/// toolOutputWithDiff appends a unified diff to the tool result text so
/// the model sees the change inside the tool output.
fn text_with_diff(summary: String, diff: &str) -> String {
    if diff.is_empty() {
        summary
    } else {
        format!("{summary}\n\n{diff}")
    }
}

pub async fn execute_write_file(arguments: &str, ctx: &ToolCtx<'_>) -> ToolOutcome {
    #[derive(serde::Deserialize, Default)]
    struct Args {
        #[serde(default)]
        path: String,
        #[serde(default)]
        content: String,
    }
    if arguments.trim().is_empty() {
        return ToolOutcome::from_text(crate::exec::empty_arguments_msg("write_file"));
    }
    let args: Args = match serde_json::from_str(arguments) {
        Ok(a) => a,
        Err(e) => return parse_err(e),
    };
    let abs = crate::exec::resolve_tool_path(&ctx.cwd, &args.path);
    if let Err(msg) = gate_fs_write(ctx, &abs).await {
        return ToolOutcome {
            text: msg,
            ..Default::default()
        };
    }
    let path = abs;
    let display = args.path.clone();
    let content = args.content.clone();
    let lock_target = path.display().to_string();
    let result = with_file_lock(&lock_target, move || {
        let existing = match std::fs::read(&path) {
            Ok(c) => Some(c),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return ToolOutcome {
                    text: format!("error reading file: {e}"),
                    ..Default::default()
                }
            }
        };
        if let Some(existing) = &existing {
            if let Err(msg) = observe_or_drift(ctx, &path, existing) {
                return ToolOutcome {
                    text: msg,
                    ..Default::default()
                };
            }
        }
        if let Err(e) = write_file_atomic(&path.display().to_string(), content.as_bytes()) {
            return ToolOutcome {
                text: format!("error writing file: {e}"),
                ..Default::default()
            };
        }
        remember_file(ctx, &path, content.as_bytes());
        let prior = existing.unwrap_or_default();
        let diff = file_diff(&display, &prior, content.as_bytes());
        outcome(
            format!("wrote {} bytes to {}", content.len(), display),
            diff,
        )
    });
    match result {
        Ok(out) => out,
        Err(msg) => ToolOutcome {
            text: msg,
            ..Default::default()
        },
    }
}

pub async fn execute_edit_file(arguments: &str, ctx: &ToolCtx<'_>) -> ToolOutcome {
    #[derive(serde::Deserialize, Default)]
    struct Args {
        #[serde(default)]
        path: String,
        #[serde(default)]
        old_text: String,
        #[serde(default)]
        new_text: String,
    }
    if arguments.trim().is_empty() {
        return ToolOutcome::from_text(crate::exec::empty_arguments_msg("edit_file"));
    }
    let args: Args = match serde_json::from_str(arguments) {
        Ok(a) => a,
        Err(e) => return parse_err(e),
    };
    let abs = crate::exec::resolve_tool_path(&ctx.cwd, &args.path);
    if let Err(msg) = gate_fs_write(ctx, &abs).await {
        return ToolOutcome {
            text: msg,
            ..Default::default()
        };
    }
    let path = abs;
    let display = args.path.clone();
    let old_text = args.old_text.clone();
    let new_text = args.new_text.clone();
    let lock_target = path.display().to_string();
    let result = with_file_lock(&lock_target, move || {
        let content = match std::fs::read(&path) {
            Ok(c) => c,
            Err(e) => {
                return ToolOutcome {
                    text: format!("error reading file: {e}"),
                    ..Default::default()
                }
            }
        };
        if let Err(msg) = observe_or_drift(ctx, &path, &content) {
            return ToolOutcome {
                text: msg,
                ..Default::default()
            };
        }
        let text = String::from_utf8_lossy(&content).to_string();
        let count = text.matches(&old_text).count();
        if count == 0 {
            return ToolOutcome {
                text: old_text_not_found_error(&text, &old_text),
                ..Default::default()
            };
        }
        if count > 1 {
            return ToolOutcome {
                text: old_text_duplicate_error(&text, &old_text, count),
                ..Default::default()
            };
        }
        let updated = text.replacen(&old_text, &new_text, 1);
        if let Err(e) = write_file_atomic(&path.display().to_string(), updated.as_bytes()) {
            return ToolOutcome {
                text: format!("error writing file: {e}"),
                ..Default::default()
            };
        }
        remember_file(ctx, &path, updated.as_bytes());
        let diff = file_diff(&display, &content, updated.as_bytes());
        outcome(
            format!(
                "edited {}: replaced {} bytes with {} bytes",
                display,
                old_text.len(),
                new_text.len()
            ),
            diff,
        )
    });
    match result {
        Ok(out) => out,
        Err(msg) => ToolOutcome {
            text: msg,
            ..Default::default()
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::test_support::*;
    use atom_sandbox::approvals::{AutoApprover, Decision};

    async fn assert_read_ok(ctx: &ToolCtx<'_>, path: &Path) {
        let out = crate::read_file::execute_read_file(
            &serde_json::json!({"path": path.display().to_string()}).to_string(),
            ctx,
        );
        assert!(!out.text.starts_with("error:"), "read_file: {}", out.text);
    }

    async fn edit(ctx: &ToolCtx<'_>, path: &Path, old: &str, new: &str) -> ToolOutcome {
        execute_edit_file(
            &serde_json::json!({
                "path": path.display().to_string(),
                "old_text": old,
                "new_text": new,
            })
            .to_string(),
            ctx,
        )
        .await
    }

    #[test]
    fn file_seen_evicts_least_recent_entry() {
        let dir = tempfile::tempdir().unwrap();
        let seen = FileSeen::new();
        let paths: Vec<PathBuf> = (0..MAX_FILE_SEEN_ENTRIES)
            .map(|i| dir.path().join(format!("{i}.txt")))
            .collect();
        for path in &paths {
            seen.remember(path, b"x");
        }

        assert!(seen.seen(&paths[0]).is_some(), "lookup must promote entry");
        seen.remember(&dir.path().join("extra.txt"), b"x");

        assert!(seen.seen(&paths[0]).is_some());
        assert!(
            seen.seen(&paths[1]).is_none(),
            "oldest entry was not evicted"
        );
        assert_eq!(
            seen.cache_usage(),
            (MAX_FILE_SEEN_ENTRIES, MAX_FILE_SEEN_ENTRIES)
        );
    }

    #[test]
    fn file_seen_bounds_and_reaccounts_retained_bytes() {
        let dir = tempfile::tempdir().unwrap();
        let seen = FileSeen::new();
        let a = dir.path().join("a.txt");
        let b = dir.path().join("b.txt");
        let chunk = vec![b'a'; MAX_FILE_SEEN_BYTES / 2 + 1];

        seen.remember(&a, &chunk);
        seen.remember(&b, &chunk);
        assert!(seen.seen(&a).is_none(), "byte pressure must evict the LRU");
        assert!(seen.seen(&b).is_some());
        assert_eq!(seen.cache_usage(), (1, chunk.len()));

        seen.remember(&b, b"small");
        assert_eq!(seen.cache_usage(), (1, 5));
    }

    #[test]
    fn oversized_file_keeps_hash_and_degrades_drift_diagnostic() {
        let env = FileEnv::new();
        let path = env.ws.path().join("large.txt");
        let data = vec![b'a'; MAX_FILE_SEEN_BYTES + 1];
        env.seen.remember(&path, &data);

        let entry = env.seen.seen(&path).unwrap();
        assert_eq!(entry.hash, sha256_hash(&data));
        assert!(entry.data.is_none());
        assert_eq!(env.seen.cache_usage(), (1, 0));
        assert!(observe_or_drift(&env.ctx(), &path, &data).is_ok());

        let mut changed = data;
        changed[0] = b'b';
        let err = observe_or_drift(&env.ctx(), &path, &changed).unwrap_err();
        assert!(err.starts_with("error: file changed since last observation."));
        assert!(err.contains("no diff is available"));
        assert!(observe_or_drift(&env.ctx(), &path, &changed).is_ok());
        assert_eq!(env.seen.cache_usage(), (1, 0));
    }

    #[tokio::test]
    async fn edit_requires_read() {
        let env = FileEnv::new();
        let path = env.ws.path().join("greet.go");
        std::fs::write(&path, "package greet\n").unwrap();

        let out = edit(&env.ctx(), &path, "package greet", "package hello").await;
        assert!(
            out.text
                .starts_with("error: file has not been read in this session."),
            "{}",
            out.text
        );
        assert_eq!(out.diff, "");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "package greet\n");
    }

    #[tokio::test]
    async fn edit_applies_when_unchanged_and_second_needs_no_read() {
        let env = FileEnv::new();
        let path = env.ws.path().join("greet.go");
        std::fs::write(
            &path,
            "func Hello() { return \"hi\" }\nfunc Bye() { return \"bye\" }\n",
        )
        .unwrap();
        assert_read_ok(&env.ctx(), &path).await;

        let out = edit(&env.ctx(), &path, "return \"hi\"", "return \"hello\"").await;
        assert!(out.text.starts_with("edited"), "{}", out.text);

        let out = edit(&env.ctx(), &path, "return \"bye\"", "return \"goodbye\"").await;
        assert!(out.text.starts_with("edited"), "{}", out.text);

        let got = std::fs::read_to_string(&path).unwrap();
        assert!(got.contains("return \"hello\"") && got.contains("return \"goodbye\""));
    }

    #[tokio::test]
    async fn edit_drift_returns_compact_diff_then_retry_applies() {
        let env = FileEnv::new();
        let path = env.ws.path().join("greet.go");
        std::fs::write(
            &path,
            "func Hello() { return \"hi\" }\nfunc Bye() { return \"bye\" }\n",
        )
        .unwrap();
        assert_read_ok(&env.ctx(), &path).await;

        std::fs::write(
            &path,
            "func Hello() { return \"hi\" }\nfunc Bye() { return \"goodbye\" }\n",
        )
        .unwrap();

        let out = edit(&env.ctx(), &path, "return \"hi\"", "return \"hello\"").await;
        assert!(
            out.text
                .starts_with("error: file changed since last observation."),
            "{}",
            out.text
        );
        assert_eq!(out.diff, "");
        assert!(
            out.text.contains("-func Bye() { return \"bye\" }")
                && out.text.contains("+func Bye() { return \"goodbye\" }"),
            "{}",
            out.text
        );
        assert!(!out.text.contains("package "), "{}", out.text);

        let out = edit(&env.ctx(), &path, "return \"hi\"", "return \"hello\"").await;
        assert!(out.text.starts_with("edited"), "{}", out.text);
    }

    #[tokio::test]
    async fn edit_old_text_not_found_shows_nearby_window() {
        let env = FileEnv::new();
        let path = env.ws.path().join("greet.go");
        std::fs::write(&path, "func Hello() string {\n\treturn \"hello\"\n}\n").unwrap();
        assert_read_ok(&env.ctx(), &path).await;

        let out = edit(&env.ctx(), &path, "return \"hi\"", "return \"hey\"").await;
        assert!(
            out.text.starts_with("error: old_text not found."),
            "{}",
            out.text
        );
        assert!(out.text.contains("return \"hello\""), "{}", out.text);
    }

    #[tokio::test]
    async fn edit_old_text_duplicate_lists_hits() {
        let env = FileEnv::new();
        let path = env.ws.path().join("greet.go");
        std::fs::write(
            &path,
            "func Hello() { return \"hello\" }\nfunc HelloAgain() { return \"hello\" }\n",
        )
        .unwrap();
        assert_read_ok(&env.ctx(), &path).await;

        let out = edit(&env.ctx(), &path, "return \"hello\"", "return \"hi\"").await;
        assert!(
            out.text.starts_with("error: old_text found 2 times."),
            "{}",
            out.text
        );
        assert!(
            out.text.contains("line 1:") && out.text.contains("line 2:"),
            "{}",
            out.text
        );
    }

    #[tokio::test]
    async fn write_existing_requires_read_and_detects_drift() {
        let env = FileEnv::new();
        let path = env.ws.path().join("a.txt");
        std::fs::write(&path, "old\n").unwrap();

        let out = execute_write_file(
            &serde_json::json!({"path": path.display().to_string(), "content": "new\n"})
                .to_string(),
            &env.ctx(),
        )
        .await;
        assert!(
            out.text
                .starts_with("error: file has not been read in this session."),
            "{}",
            out.text
        );

        assert_read_ok(&env.ctx(), &path).await;
        std::fs::write(&path, "changed\n").unwrap();

        let out = execute_write_file(
            &serde_json::json!({"path": path.display().to_string(), "content": "new\n"})
                .to_string(),
            &env.ctx(),
        )
        .await;
        assert!(
            out.text
                .starts_with("error: file changed since last observation."),
            "{}",
            out.text
        );
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "changed\n");

        // After refreshing observation the write applies and returns a diff.
        assert_read_ok(&env.ctx(), &path).await;
        let out = execute_write_file(
            &serde_json::json!({"path": path.display().to_string(), "content": "new\n"})
                .to_string(),
            &env.ctx(),
        )
        .await;
        assert!(out.text.starts_with("wrote 4 bytes to "), "{}", out.text);
        assert!(
            out.diff.contains("-changed\n") && out.diff.contains("+new\n"),
            "{}",
            out.diff
        );
        assert!(out.text.contains(out.diff.trim_end()));
    }

    #[tokio::test]
    async fn write_new_file_needs_no_read() {
        let env = FileEnv::new();
        let path = env.ws.path().join("fresh.txt");
        let out = execute_write_file(
            &serde_json::json!({"path": path.display().to_string(), "content": "abc"}).to_string(),
            &env.ctx(),
        )
        .await;
        assert!(out.text.starts_with("wrote 3 bytes to "), "{}", out.text);
    }

    #[tokio::test]
    async fn lock_busy_returns_busy_error() {
        let env = FileEnv::new();
        let path = env.ws.path().join("a.txt");
        std::fs::write(&path, "hello\n").unwrap();
        assert_read_ok(&env.ctx(), &path).await;

        // Hold the same lock another "process" would take.
        let lock = lock_file_path(&path.display().to_string());
        let held = std::fs::OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&lock)
            .unwrap();
        let fd = std::os::unix::io::AsRawFd::as_raw_fd(&held);
        assert_eq!(unsafe { libc::flock(fd, libc::LOCK_EX | libc::LOCK_NB) }, 0);

        let err = with_file_lock_timeout(
            &default_locks_dir(),
            &path.display().to_string(),
            Duration::from_millis(80),
            || "ran",
        )
        .unwrap_err();
        assert_eq!(err, file_busy_error());

        unsafe { libc::flock(fd, libc::LOCK_UN) };

        // Once free, the closure runs.
        let ok = with_file_lock_timeout(
            &default_locks_dir(),
            &path.display().to_string(),
            Duration::from_millis(80),
            || "ran",
        )
        .unwrap();
        assert_eq!(ok, "ran");
    }

    #[tokio::test]
    async fn outside_cwd_write_requires_approval() {
        let outside = tempfile::tempdir().unwrap();
        let env = FileEnv::new(); // ws != outside
        let path = outside.path().join("elsewhere.txt");

        let denied = execute_write_file(
            &serde_json::json!({"path": path.display().to_string(), "content": "x"}).to_string(),
            &env.ctx_with(&AutoApprover(Decision::Deny)),
        )
        .await;
        assert!(
            denied
                .text
                .starts_with("error: write outside the workspace was not approved"),
            "{}",
            denied.text
        );
        assert!(!path.exists());

        let allowed = execute_write_file(
            &serde_json::json!({"path": path.display().to_string(), "content": "x"}).to_string(),
            &env.ctx_with(&AutoApprover(Decision::AllowSession)),
        )
        .await;
        assert!(allowed.text.starts_with("wrote "), "{}", allowed.text);
        assert!(path.exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn workspace_symlink_to_outside_requires_approval() {
        let outside = tempfile::tempdir().unwrap();
        let env = FileEnv::new();
        std::os::unix::fs::symlink(outside.path(), env.ws.path().join("outside-link")).unwrap();

        let denied = execute_write_file(
            r#"{"path":"outside-link/new.txt","content":"x"}"#,
            &env.ctx_with(&AutoApprover(Decision::Deny)),
        )
        .await;

        assert!(
            denied.text.contains("outside the workspace"),
            "{}",
            denied.text
        );
        assert!(!outside.path().join("new.txt").exists());
    }

    // ---- pure helpers ported from file_edit_test coverage ----

    #[test]
    fn cap_diff_lines_truncates() {
        let diff = (0..100)
            .map(|i| format!("line{i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (capped, trunc) = cap_diff_lines(&diff, 80);
        assert!(trunc);
        assert_eq!(capped.lines().count(), 80);
        let (whole, trunc) = cap_diff_lines("a\nb", 80);
        assert!(!trunc);
        assert_eq!(whole, "a\nb");
    }

    #[test]
    fn first_changed_line_parses_hunk_header() {
        let diff = "@@ -12,3 +12,3 @@\n-x\n+y\n";
        assert_eq!(first_changed_line_offset(diff), 11);
        assert_eq!(first_changed_line_offset("no hunks"), 0);
    }

    #[test]
    fn read_file_output_counts_and_continues() {
        assert_eq!(count_file_lines(""), 0);
        assert_eq!(count_file_lines("a\nb"), 2);
        assert_eq!(count_file_lines("a\nb\n"), 2);
        let full = "one\ntwo\nthree\n";
        assert_eq!(read_file_output(full, 0, 0), full);
        let got = read_file_output(full, 1, 1);
        assert_eq!(got, "two\n[1 more lines. Use offset=2 to continue.]");
    }
}
