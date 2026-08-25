//! Session store and persistence, ported from session.go.

use crate::types::{Message, StreamUsage};
use crate::util::{add_stream_usage, sha256_hash};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DelegateStatus {
    Queued,
    Working,
    Sandbox,
    Error,
    #[default]
    Done,
    Cancelled,
}

impl DelegateStatus {
    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Working | Self::Sandbox)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Working => "working",
            Self::Sandbox => "sandbox",
            Self::Error => "error",
            Self::Done => "done",
            Self::Cancelled => "cancelled",
        }
    }
}

fn is_done(status: &DelegateStatus) -> bool {
    *status == DelegateStatus::Done
}

/// Session is a persisted conversation. Messages hold the conversation
/// history without system instructions, which are loaded fresh from disk
/// when the session is created. Model is set at creation time and can be
/// changed later (the TUI's /model command updates it); Cwd is fixed for
/// the life of the session. Usage is the token count of the session's
/// latest model request, reported by the provider. Compaction uses this
/// last-round snapshot (PromptTokens). Status-bar Input/Output and cache
/// hit rate are session totals from usageForDisplay; the context meter
/// still uses this snapshot's TotalTokens.
/// CompactionSummary and CompactedThrough fold older turns into a short
/// brief for later model requests; the full transcript stays in Messages.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Session {
    pub id: String,
    pub title: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub title_generated: bool,
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    pub messages: Vec<Message>,
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    pub cwd: String,
    #[serde(default, deserialize_with = "crate::serde_null::null_as_default")]
    pub instructions: Vec<Message>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<StreamUsage>,
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        rename = "compaction_summary"
    )]
    pub compaction_summary: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub compacted_through: i64,
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        rename = "parent_id"
    )]
    pub parent_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub thinking: String,
    /// Cancelled is set when the parent explicitly kills a dispatched
    /// subagent via `dispatch action=cancel`. Killed subagents are dropped
    /// from the parent's children listing; alive ones stay listed even
    /// after their turn finishes.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
    #[serde(default, skip_serializing_if = "is_done")]
    pub status: DelegateStatus,
    #[serde(default, skip_serializing_if = "String::is_empty", rename = "batch_id")]
    pub batch_id: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub batch_index: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn is_zero(n: &i64) -> bool {
    *n == 0
}

impl Default for Session {
    fn default() -> Self {
        let now = Utc::now();
        Session {
            id: String::new(),
            title: String::new(),
            title_generated: false,
            messages: Vec::new(),
            model: String::new(),
            provider: String::new(),
            cwd: String::new(),
            instructions: Vec::new(),
            usage: None,
            compaction_summary: String::new(),
            compacted_through: 0,
            parent_id: String::new(),
            thinking: String::new(),
            cancelled: false,
            status: DelegateStatus::Done,
            batch_id: String::new(),
            batch_index: 0,
            created_at: now,
            updated_at: now,
        }
    }
}

impl Session {
    /// info returns a SessionInfo summary for a session. When no title
    /// has been assigned, the first user message becomes the display
    /// name so sessions are recognizable in the picker.
    pub fn info(&self) -> SessionInfo {
        let mut title = self.title.clone();
        if title.is_empty() {
            title = session_name(&self.messages);
        }
        SessionInfo {
            id: self.id.clone(),
            title,
            model: self.model.clone(),
            provider: self.provider.clone(),
            message_count: self.messages.len(),
            usage: usage_for_display(self.usage.as_ref(), Some(self), None),
            parent_id: self.parent_id.clone(),
            thinking: self.thinking.clone(),
            cancelled: self.cancelled,
            status: self.status,
            batch_id: self.batch_id.clone(),
            batch_index: self.batch_index,
            created_at: self.created_at,
            updated_at: self.updated_at,
        }
    }
}

/// Last file bytes observed by read_file / successful edit or write, or
/// a "file changed" check. Not persisted; kept by the store alongside
/// the session (Go keeps these on the Session struct behind a mutex).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FileSeenEntry {
    pub hash: String,
    pub data: Vec<u8>,
}

/// SessionInfo summarizes a session for listing.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInfo {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub model: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider: String,
    #[serde(default)]
    pub message_count: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub usage: Option<StreamUsage>,
    #[serde(
        default,
        skip_serializing_if = "String::is_empty",
        rename = "parent_id"
    )]
    pub parent_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub thinking: String,
    /// Cancelled marks a dispatched subagent that the parent explicitly
    /// killed. See Session::cancelled.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cancelled: bool,
    #[serde(default, skip_serializing_if = "is_done")]
    pub status: DelegateStatus,
    #[serde(default, skip_serializing_if = "String::is_empty", rename = "batch_id")]
    pub batch_id: String,
    #[serde(default, skip_serializing_if = "is_zero")]
    pub batch_index: i64,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// atom data directory, honouring XDG_DATA_HOME (~/.local/share/atom).
pub fn data_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share")))
        .unwrap_or_else(|| std::env::temp_dir().join("atom"));
    let d = base.join("atom");
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Unix socket path for the session server.
pub fn socket_path() -> PathBuf {
    data_dir().join("atom.sock")
}

/// Directory where session JSON files live.
pub fn sessions_dir() -> PathBuf {
    data_dir().join("sessions")
}

/// newSessionID: 16-char random hex.
pub fn new_session_id() -> String {
    use rand::Rng;
    let b: [u8; 8] = rand::thread_rng().gen();
    hex::encode(b)
}

/// SessionStore keeps a compact session index in memory and persists each
/// full session as a JSON file in its directory. Full transcripts are read
/// on demand and discarded after each operation.
pub struct SessionStore {
    dir: PathBuf,
    index: Mutex<HashMap<String, SessionInfo>>,
    /// Per-session file observations (session ID -> canonical path ->
    /// entry). Mirrors Session.fileSeen; never persisted.
    file_seen: Mutex<HashMap<String, HashMap<String, FileSeenEntry>>>,
}

impl SessionStore {
    /// Creates a store rooted at the default sessions directory,
    /// loading existing sessions from disk.
    pub fn open() -> anyhow::Result<Self> {
        Self::open_in_dir(sessions_dir())
    }

    /// Dir-parameterized constructor (tests inject a temp dir instead of
    /// mutating XDG_DATA_HOME).
    pub fn open_in_dir(dir: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let dir = dir.into();
        std::fs::create_dir_all(&dir)?;
        let s = SessionStore {
            dir,
            index: Mutex::new(HashMap::new()),
            file_seen: Mutex::new(HashMap::new()),
        };
        s.load_all();
        Ok(s)
    }

    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }

    /// loadAll builds the compact index, discarding each full session once
    /// its listing metadata has been extracted.
    fn load_all(&self) {
        let entries = match std::fs::read_dir(&self.dir) {
            Ok(e) => e,
            Err(_) => return,
        };
        let mut index = self.index.lock().unwrap();
        for e in entries.flatten() {
            let path = e.path();
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            if !e.file_name().to_string_lossy().ends_with(".json") {
                continue;
            }
            let b = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let mut info = match session_info_from_slice(&b) {
                Ok(info) => info,
                Err(_) => continue,
            };
            // Old files may predate persisted fallback titles. Preserve the
            // recognizable first-message title for that rare case without
            // allocating every transcript during normal startup.
            if info.title.is_empty() && info.message_count > 0 {
                if let Ok(sess) = session_from_slice(&b) {
                    info = sess.info();
                }
            }
            index.insert(info.id.clone(), info);
        }
    }

    /// Create makes a new session with the given model, cwd, and
    /// instructions, persists it, and returns it.
    pub fn create(&self, model: &str, cwd: &str, instructions: Vec<Message>) -> Session {
        let sess = Session {
            id: new_session_id(),
            model: model.into(),
            cwd: cwd.into(),
            instructions,
            ..Default::default()
        };
        let mut index = self.index.lock().unwrap();
        self.save(&sess);
        index.insert(sess.id.clone(), sess.info());
        sess
    }

    /// CreateChild makes a new session parented to parentID, with its
    /// own model, thinking level, and instructions. Title is the given
    /// title, or the first line of the last instruction if title is
    /// empty. Empty children are kept so the TUI can list them.
    pub fn create_child(
        &self,
        parent_id: &str,
        model: &str,
        cwd: &str,
        thinking: &str,
        title: &str,
        instructions: Vec<Message>,
    ) -> Session {
        let mut title = title.to_string();
        if title.is_empty() {
            if let Some(last) = instructions.last() {
                title = first_line_trunc(&last.content, 60);
            }
        }
        let sess = Session {
            id: new_session_id(),
            title,
            model: model.into(),
            cwd: cwd.into(),
            instructions: instructions.clone(),
            parent_id: parent_id.into(),
            thinking: thinking.into(),
            status: DelegateStatus::Queued,
            ..Default::default()
        };
        let mut index = self.index.lock().unwrap();
        self.save(&sess);
        index.insert(sess.id.clone(), sess.info());
        sess
    }

    /// IsDescendantOf reports whether childID is parented (directly or
    /// nested) under ancestorID. A session is not a descendant of itself.
    pub fn is_descendant_of(&self, child_id: &str, ancestor_id: &str) -> bool {
        if child_id.is_empty() || ancestor_id.is_empty() || child_id == ancestor_id {
            return false;
        }
        let index = self.index.lock().unwrap();
        let mut seen = std::collections::HashSet::new();
        let mut id = child_id.to_string();
        for _ in 0..32 {
            if seen.contains(&id) {
                return false;
            }
            seen.insert(id.clone());
            let info = match index.get(&id) {
                Some(s) => s,
                None => return false,
            };
            if info.parent_id == ancestor_id {
                return true;
            }
            if info.parent_id.is_empty() {
                return false;
            }
            id = info.parent_id.clone();
        }
        false
    }

    /// Children returns sessions whose ParentID matches, newest
    /// CreatedAt first. Empty (0-message) children are included.
    pub fn children(&self, parent_id: &str) -> Vec<Session> {
        let index = self.index.lock().unwrap();
        let mut children: Vec<&SessionInfo> = index
            .values()
            .filter(|info| info.parent_id == parent_id)
            .collect();
        children.sort_by_key(|info| std::cmp::Reverse(info.created_at));
        children
            .into_iter()
            .filter_map(|info| self.load(&info.id))
            .collect()
    }

    /// Compact child listing for status UIs and wait loops. This avoids
    /// loading every transcript when only lifecycle metadata is needed.
    pub fn children_info(&self, parent_id: &str) -> Vec<SessionInfo> {
        let mut children: Vec<SessionInfo> = self
            .index
            .lock()
            .unwrap()
            .values()
            .filter(|info| info.parent_id == parent_id)
            .cloned()
            .collect();
        children.sort_by_key(|info| std::cmp::Reverse(info.created_at));
        children
    }

    /// Get loads a full session by ID, or returns None if not found.
    pub fn get(&self, id: &str) -> Option<Session> {
        let index = self.index.lock().unwrap();
        index.contains_key(id).then(|| self.load(id)).flatten()
    }

    /// List returns all sessions sorted by UpdatedAt descending (most
    /// recent first). Full sessions are loaded from disk for the caller but
    /// are not retained by the store.
    pub fn list(&self) -> Vec<Session> {
        let index = self.index.lock().unwrap();
        let mut infos: Vec<&SessionInfo> = index.values().collect();
        infos.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        infos
            .into_iter()
            .filter_map(|info| self.load(&info.id))
            .collect()
    }

    /// ListInfo returns compact listing metadata without loading session
    /// transcripts, sorted by UpdatedAt descending.
    pub fn list_info(&self) -> Vec<SessionInfo> {
        let mut list: Vec<SessionInfo> = self.index.lock().unwrap().values().cloned().collect();
        list.sort_by(|a, b| b.updated_at.cmp(&a.updated_at));
        list
    }

    /// Update saves the messages for a session and touches its UpdatedAt.
    /// If title is non-empty, it replaces the session's title unless an
    /// LLM title has already been stored (TitleGenerated).
    pub fn update(&self, id: &str, messages: Vec<Message>, title: &str) {
        self.mutate(id, |sess| {
            sess.messages = messages;
            if !title.is_empty() && !sess.title_generated {
                sess.title = title.to_string();
            }
            sess.updated_at = Utc::now();
        });
    }

    /// UpdateTitle sets Title, marks TitleGenerated, and saves. Messages
    /// are left unchanged.
    pub fn update_title(&self, id: &str, title: &str) {
        self.mutate(id, |sess| {
            sess.title = title.to_string();
            sess.title_generated = true;
            sess.updated_at = Utc::now();
        });
    }

    /// UpdateModel changes the model that answers future messages in a
    /// session. The conversation history is kept; only the model used
    /// for new turns changes. It's a no-op if the session doesn't exist.
    pub fn update_model(&self, id: &str, model: &str) {
        self.mutate(id, |sess| {
            sess.model = model.to_string();
            sess.updated_at = Utc::now();
        });
    }

    /// UpdateProvider records the backend selected for a dispatched child.
    pub fn update_provider(&self, id: &str, provider: &str) {
        self.mutate(id, |sess| {
            sess.provider = provider.to_string();
            sess.updated_at = Utc::now();
        });
    }

    /// UpdateThinking stores the TUI's current reasoning_effort for this
    /// session so Tab/Ctrl+T cycles survive reloads after a turn saves.
    pub fn update_thinking(&self, id: &str, thinking: &str) {
        self.mutate(id, |sess| {
            sess.thinking = thinking.to_string();
            sess.updated_at = Utc::now();
        });
    }

    /// SetCancelled records whether a dispatched subagent was explicitly
    /// killed by its parent (true) or revived by a follow-up (false).
    pub fn set_cancelled(&self, id: &str, cancelled: bool) -> bool {
        self.mutate(id, |sess| {
            sess.cancelled = cancelled;
            sess.updated_at = Utc::now();
        })
    }

    pub fn update_delegate_status(&self, id: &str, status: DelegateStatus) -> bool {
        self.mutate(id, |sess| {
            sess.status = status;
            sess.updated_at = Utc::now();
        })
    }

    pub fn update_delegate_batch(&self, id: &str, batch_id: &str, batch_index: usize) -> bool {
        self.mutate(id, |sess| {
            sess.batch_id = batch_id.to_string();
            sess.batch_index = batch_index as i64;
            sess.updated_at = Utc::now();
        })
    }

    /// Turns left active by a daemon exit cannot still be running after a
    /// fresh server starts. Mark them as errors instead of showing a stuck
    /// spinner forever.
    pub fn reconcile_delegate_statuses(&self) {
        let ids: Vec<String> = self
            .index
            .lock()
            .unwrap()
            .values()
            .filter(|info| !info.parent_id.is_empty() && info.status.is_active())
            .map(|info| info.id.clone())
            .collect();
        for id in ids {
            self.update_delegate_status(&id, DelegateStatus::Error);
        }
    }

    /// Delete removes a session from memory and disk.
    pub fn delete(&self, id: &str) {
        self.index.lock().unwrap().remove(id);
        self.file_seen.lock().unwrap().remove(id);
        let _ = std::fs::remove_file(self.dir.join(format!("{id}.json")));
    }

    /// modify loads the stored session, applies f, and persists the result.
    /// Saving here preserves fields across the server's sync_back then
    /// update flow even though full sessions are no longer held in memory.
    pub fn modify<F: FnOnce(&mut Session)>(&self, id: &str, f: F) -> bool {
        self.mutate(id, f)
    }

    /// rememberFile records the last observed bytes of a file for a
    /// session, keyed by canonical path (Go file_edit.go).
    pub fn remember_file(&self, session_id: &str, path: &str, data: &[u8]) {
        let key = canonical_file_path(path);
        let entry = FileSeenEntry {
            hash: sha256_hash(data),
            data: data.to_vec(),
        };
        self.file_seen
            .lock()
            .unwrap()
            .entry(session_id.to_string())
            .or_default()
            .insert(key, entry);
    }

    /// seenFile returns the last observed bytes for path, if any.
    pub fn seen_file(&self, session_id: &str, path: &str) -> Option<FileSeenEntry> {
        let key = canonical_file_path(path);
        self.file_seen
            .lock()
            .unwrap()
            .get(session_id)
            .and_then(|m| m.get(&key))
            .cloned()
    }

    /// load reads a full session from its JSON file. Callers hold the index
    /// lock so a concurrent mutation cannot interleave with the read.
    fn load(&self, id: &str) -> Option<Session> {
        let b = std::fs::read(self.dir.join(format!("{id}.json"))).ok()?;
        session_from_slice(&b).ok()
    }

    /// mutate serializes a full-session read/modify/write with all other
    /// session operations and refreshes the compact index.
    fn mutate<F: FnOnce(&mut Session)>(&self, id: &str, f: F) -> bool {
        let mut index = self.index.lock().unwrap();
        if !index.contains_key(id) {
            return false;
        }
        let Some(mut sess) = self.load(id) else {
            return false;
        };
        f(&mut sess);
        self.save(&sess);
        index.insert(id.to_string(), sess.info());
        true
    }

    /// save writes a session to its Go-compatible JSON file. Callers hold
    /// the index lock to keep disk and metadata updates ordered.
    fn save(&self, sess: &Session) {
        let b = match serde_json::to_string_pretty(sess) {
            Ok(b) => b,
            Err(_) => return,
        };
        let _ = std::fs::write(self.dir.join(format!("{}.json", sess.id)), b);
    }
}

/// Session files contain StreamUsage's canonical persisted fields, while
/// provider responses use nested reasoning and total_cost fields. Normalize
/// the persisted aliases before invoking the provider-oriented deserializer.
fn session_from_slice(b: &[u8]) -> serde_json::Result<Session> {
    let mut value: serde_json::Value = serde_json::from_slice(b)?;
    if let Some(obj) = value.as_object_mut() {
        normalize_persisted_usage(obj.get_mut("usage"));
        if let Some(serde_json::Value::Array(messages)) = obj.get_mut("messages") {
            for message in messages {
                normalize_persisted_usage(message.get_mut("usage"));
            }
        }
    }
    serde_json::from_value(value)
}

/// Reads only listing metadata from a session file. `IgnoredAny` lets serde
/// validate and skip message/instruction bodies without allocating their
/// strings, keeping daemon startup memory independent of transcript size.
fn session_info_from_slice(b: &[u8]) -> serde_json::Result<SessionInfo> {
    #[derive(Deserialize, Default)]
    #[serde(default)]
    struct IndexUsage {
        prompt_tokens: i64,
        completion_tokens: i64,
        total_tokens: i64,
        reasoning_tokens: i64,
        cache_read_tokens: i64,
        cache_write_tokens: i64,
        cost: f64,
    }

    impl From<IndexUsage> for StreamUsage {
        fn from(usage: IndexUsage) -> Self {
            StreamUsage {
                prompt_tokens: usage.prompt_tokens,
                completion_tokens: usage.completion_tokens,
                total_tokens: usage.total_tokens,
                reasoning_tokens: usage.reasoning_tokens,
                cache_read_tokens: usage.cache_read_tokens,
                cache_write_tokens: usage.cache_write_tokens,
                cost: usage.cost,
                prompt_tokens_all: 0,
            }
        }
    }

    #[derive(Deserialize)]
    struct IndexRecord {
        id: String,
        #[serde(default)]
        title: String,
        #[serde(
            default,
            deserialize_with = "crate::serde_null::null_elements_as_default"
        )]
        messages: Vec<serde::de::IgnoredAny>,
        #[serde(default)]
        model: String,
        #[serde(default)]
        provider: String,
        #[serde(default)]
        usage: Option<IndexUsage>,
        #[serde(default, rename = "parent_id")]
        parent_id: String,
        #[serde(default)]
        thinking: String,
        #[serde(default)]
        status: DelegateStatus,
        #[serde(default, rename = "batch_id")]
        batch_id: String,
        #[serde(default)]
        batch_index: i64,
        #[serde(default)]
        cancelled: bool,
        created_at: DateTime<Utc>,
        updated_at: DateTime<Utc>,
    }

    let record: IndexRecord = serde_json::from_slice(b)?;
    Ok(SessionInfo {
        id: record.id,
        title: record.title,
        model: record.model,
        provider: record.provider,
        message_count: record.messages.len(),
        usage: record.usage.map(Into::into),
        parent_id: record.parent_id,
        thinking: record.thinking,
        cancelled: record.cancelled,
        status: record.status,
        batch_id: record.batch_id,
        batch_index: record.batch_index,
        created_at: record.created_at,
        updated_at: record.updated_at,
    })
}

fn normalize_persisted_usage(value: Option<&mut serde_json::Value>) {
    let Some(obj) = value.and_then(serde_json::Value::as_object_mut) else {
        return;
    };
    if let Some(reasoning) = obj.get("reasoning_tokens").cloned() {
        let details = obj
            .entry("completion_tokens_details")
            .or_insert_with(|| serde_json::json!({}));
        if let Some(details) = details.as_object_mut() {
            details.entry("reasoning_tokens").or_insert(reasoning);
        }
    }
    if let Some(cost) = obj.get("cost").cloned() {
        obj.entry("total_cost").or_insert(cost);
    }
}

/// canonicalFilePath resolves a path to an absolute, symlink-resolved
/// form; when resolution fails (e.g. missing file) the absolute path is
/// used (Go file_edit.go).
fn canonical_file_path(path: &str) -> String {
    let p = Path::new(path);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        match std::env::current_dir() {
            Ok(cwd) => cwd.join(p),
            Err(_) => p.to_path_buf(),
        }
    };
    match std::fs::canonicalize(&abs) {
        Ok(real) => real.to_string_lossy().into_owned(),
        Err(_) => abs.to_string_lossy().into_owned(),
    }
}

/// firstLineTrunc returns the first line of s, trimmed and cut to max
/// runes (session.go variant; unlike util::first_line_trunc it does not
/// append an ellipsis).
pub fn first_line_trunc(s: &str, max: usize) -> String {
    let line = s.split('\n').next().unwrap_or(s);
    let trimmed = line.trim();
    let mut r: Vec<char> = trimmed.chars().collect();
    if max > 0 && r.len() > max {
        r.truncate(max);
    }
    r.into_iter().collect()
}

/// addStreamUsage folds src into dst. Either side may be absent; the
/// core helper covers the non-nil case.
pub fn add_usage(dst: &mut StreamUsage, src: &StreamUsage) {
    add_stream_usage(dst, src);
}

/// sumMessageUsage totals per-round provider usage stored on messages.
pub fn sum_message_usage(sess: Option<&Session>) -> StreamUsage {
    let mut sum = StreamUsage::default();
    let Some(sess) = sess else {
        return sum;
    };
    for m in &sess.messages {
        if let Some(u) = &m.usage {
            add_stream_usage(&mut sum, u);
        }
    }
    sum
}

/// usageForDisplay builds the status-bar usage object. Prompt and
/// completion are summed across every recorded round plus optional extra
/// (session Input/Output). TotalTokens stays the latest round's total
/// (current context size vs window). Cache counters and PromptTokensAll
/// stay session sums. extra is a round not yet appended to Messages.
pub fn usage_for_display(
    latest: Option<&StreamUsage>,
    sess: Option<&Session>,
    extra: Option<&StreamUsage>,
) -> Option<StreamUsage> {
    let mut sum = sum_message_usage(sess);
    if let Some(extra) = extra {
        add_stream_usage(&mut sum, extra);
    }
    let latest = match latest {
        Some(l) => l.clone(),
        None => {
            if sum.prompt_tokens == 0 && sum.total_tokens == 0 {
                return None;
            }
            sum.clone()
        }
    };
    let mut out = latest;
    if sum.prompt_tokens > 0 {
        out.prompt_tokens = sum.prompt_tokens;
        out.completion_tokens = sum.completion_tokens;
        out.cache_read_tokens = sum.cache_read_tokens;
        out.cache_write_tokens = sum.cache_write_tokens;
        out.prompt_tokens_all = sum.prompt_tokens;
    }
    Some(out)
}

/// sessionName derives a display name from a session's first user
/// message: the first line, truncated to 60 characters. Returns "" for
/// empty sessions.
pub fn session_name(messages: &[Message]) -> String {
    for msg in messages {
        if msg.role != "user" {
            continue;
        }
        let line = msg.content.split('\n').next().unwrap_or("");
        let r: Vec<char> = line.chars().collect();
        if r.len() > 60 {
            return r[..60].iter().collect();
        }
        return line.to_string();
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Unique temp dir per call (std-only; no env mutation).
    fn temp_dir(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "atom-store-test-{}-{}-{tag}",
            std::process::id(),
            new_session_id()
        ));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    /// Removes the temp dir when dropped.
    struct Cleanup(PathBuf);
    impl Cleanup {
        fn path(&self) -> &PathBuf {
            &self.0
        }
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn temp_store(tag: &str) -> (Cleanup, SessionStore) {
        let dir = temp_dir(tag);
        let store = SessionStore::open_in_dir(&dir).unwrap();
        (Cleanup(dir), store)
    }

    fn user_msg(content: &str) -> Message {
        Message {
            role: "user".into(),
            content: content.into(),
            ..Default::default()
        }
    }

    #[test]
    fn session_store_update_model() {
        let (_d, store) = temp_store("update-model");
        let sess = store.create("minimax-01", "/tmp", vec![]);
        store.update_model(&sess.id, "gpt-oss:120b-cloud");
        store.update_provider(&sess.id, "ollama");

        let got = store
            .get(&sess.id)
            .expect("session missing after update_model");
        assert_eq!(got.model, "gpt-oss:120b-cloud");
        assert_eq!(got.provider, "ollama");
        assert_eq!(got.info().provider, "ollama");
        assert!(
            got.messages.is_empty(),
            "update_model should keep the conversation"
        );
    }

    #[test]
    fn session_store_children() {
        let (_d, store) = temp_store("children");
        let parent = store.create("m", "/tmp", vec![]);
        std::thread::sleep(Duration::from_millis(5));
        let empty = store.create_child(
            &parent.id,
            "child-a",
            "/tmp",
            "high",
            "",
            vec![user_msg("First child prompt")],
        );
        std::thread::sleep(Duration::from_millis(5));
        let later = store.create_child(
            &parent.id,
            "child-b",
            "/tmp",
            "low",
            "",
            vec![user_msg("Second child\nmore")],
        );
        store.create("unrelated", "/tmp", vec![]);

        let kids = store.children(&parent.id);
        assert_eq!(kids.len(), 2, "children should include empty");
        assert_eq!(kids[0].id, later.id, "newest child first");
        assert_eq!(kids[1].id, empty.id, "older child second");
        assert_eq!(empty.title, "First child prompt");
        assert_eq!(later.title, "Second child");

        let info = empty.info();
        assert_eq!(info.parent_id, parent.id);
        assert_eq!(info.thinking, "high");
        assert!(
            store.children("missing").is_empty(),
            "unknown parent should have no children"
        );
    }

    #[test]
    fn session_store_set_cancelled() {
        let (_d, store) = temp_store("cancelled");
        let parent = store.create("m", "/tmp", vec![]);
        let child = store.create_child(
            &parent.id,
            "child-a",
            "/tmp",
            "high",
            "",
            vec![user_msg("First child prompt")],
        );

        assert!(!child.cancelled, "fresh subagent is alive");
        assert!(!child.info().cancelled);

        // An explicit kill persists on disk and through the info summary.
        assert!(store.set_cancelled(&child.id, true));
        let killed = store.get(&child.id).expect("session missing after kill");
        assert!(killed.cancelled, "kill must persist in the stored session");
        assert!(killed.info().cancelled);

        // children() still returns it (the HTTP layer filters), and a
        // follow-up explicitly revives it.
        assert_eq!(store.children(&parent.id).len(), 1);
        assert!(store.set_cancelled(&child.id, false));
        assert!(
            !store.get(&child.id).unwrap().cancelled,
            "follow-up revives a killed subagent"
        );

        assert!(
            !store.set_cancelled("missing-session", true),
            "unknown sessions are a no-op"
        );
    }

    #[test]
    fn delegate_status_and_batch_survive_reload_and_reconcile() {
        let (dir, store) = temp_store("delegate-status");
        let parent = store.create("m", "/tmp", vec![]);
        let child = store.create_child(&parent.id, "m", "/tmp", "high", "child", vec![]);
        store.update_delegate_batch(&child.id, "batch-one", 7);
        store.update_delegate_status(&child.id, DelegateStatus::Working);
        drop(store);

        let store = SessionStore::open_in_dir(dir.path()).unwrap();
        let info = &store.children_info(&parent.id)[0];
        assert_eq!(info.status, DelegateStatus::Working);
        assert_eq!(info.batch_id, "batch-one");
        assert_eq!(info.batch_index, 7);

        store.reconcile_delegate_statuses();
        assert_eq!(
            store.get(&child.id).unwrap().status,
            DelegateStatus::Error,
            "work interrupted by a daemon restart must not remain active"
        );
    }

    #[test]
    fn session_store_is_descendant_of() {
        let (_d, store) = temp_store("descendant");
        let root = store.create("m", "/tmp", vec![]);
        let child = store.create_child(&root.id, "m", "/tmp", "low", "c", vec![]);
        let grand = store.create_child(&child.id, "m", "/tmp", "low", "g", vec![]);
        let other = store.create("m", "/tmp", vec![]);
        assert!(store.is_descendant_of(&child.id, &root.id));
        assert!(store.is_descendant_of(&grand.id, &root.id));
        assert!(!store.is_descendant_of(&root.id, &child.id));
        assert!(!store.is_descendant_of(&root.id, &root.id));
        assert!(!store.is_descendant_of(&other.id, &root.id));
        assert!(!store.is_descendant_of(&child.id, &other.id));
    }

    #[test]
    fn usage_for_display_session_totals() {
        let round1 = StreamUsage {
            prompt_tokens: 1000,
            completion_tokens: 50,
            total_tokens: 1050,
            cache_read_tokens: 800,
            cache_write_tokens: 200,
            ..Default::default()
        };
        let round2 = StreamUsage {
            prompt_tokens: 1200,
            completion_tokens: 80,
            total_tokens: 1280,
            cache_read_tokens: 900,
            cache_write_tokens: 300,
            ..Default::default()
        };
        let sess = Session {
            usage: Some(round2.clone()),
            messages: vec![
                Message {
                    role: "assistant".into(),
                    usage: Some(round1),
                    ..Default::default()
                },
                Message {
                    role: "assistant".into(),
                    usage: Some(round2.clone()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };

        let got = usage_for_display(sess.usage.as_ref(), Some(&sess), None).unwrap();
        assert_eq!(got.prompt_tokens, 2200);
        assert_eq!(got.completion_tokens, 130);
        assert_eq!(got.total_tokens, 1280);
        assert_eq!(got.cache_read_tokens, 1700);
        assert_eq!(got.cache_write_tokens, 500);
        assert_eq!(got.prompt_tokens_all, 2200);
        assert_eq!(
            sess.usage.as_ref().unwrap().cache_read_tokens,
            900,
            "snapshot mutated"
        );
        assert_eq!(
            sess.usage.as_ref().unwrap().prompt_tokens,
            1200,
            "snapshot mutated"
        );

        // Current round is not on Messages yet (tool-loop usage event).
        let round3 = StreamUsage {
            prompt_tokens: 1500,
            completion_tokens: 10,
            total_tokens: 1510,
            cache_read_tokens: 1400,
            ..Default::default()
        };
        let live = usage_for_display(Some(&round3), Some(&sess), Some(&round3)).unwrap();
        assert_eq!(live.prompt_tokens, 3700);
        assert_eq!(live.completion_tokens, 140);
        assert_eq!(live.total_tokens, 1510);
        assert_eq!(live.prompt_tokens_all, 3700);
    }

    #[test]
    fn session_store_update_title() {
        let (_d, store) = temp_store("update-title");
        let sess = store.create("m", "/tmp", vec![]);
        store.update(
            &sess.id,
            vec![
                user_msg("hello"),
                Message {
                    role: "assistant".into(),
                    content: "hi".into(),
                    ..Default::default()
                },
            ],
            "fallback",
        );

        store.update_title(&sess.id, "Named by model");
        let got = store.get(&sess.id).unwrap();
        assert_eq!(got.title, "Named by model");
        assert!(got.title_generated, "TitleGenerated should be true");
        assert_eq!(got.messages.len(), 2, "update_title wiped messages");

        let mut msgs = got.messages.clone();
        msgs.push(user_msg("again"));
        store.update(&sess.id, msgs, "should not replace");
        let got = store.get(&sess.id).unwrap();
        assert_eq!(
            got.title, "Named by model",
            "Update clobbered generated title"
        );
        assert_eq!(got.messages.len(), 3);
    }

    #[test]
    fn session_store_update_model_unknown_is_noop() {
        let (_d, store) = temp_store("noop");
        store.update_model("does-not-exist", "some-model");
        assert!(
            store.list().is_empty(),
            "unknown ID must not create a session"
        );
    }

    #[test]
    fn session_store_update_thinking() {
        let (_d, store) = temp_store("thinking");
        let sess = store.create("gpt-5", "/tmp", vec![]);
        store.update_thinking(&sess.id, "low");
        let got = store.get(&sess.id).unwrap();
        assert_eq!(got.thinking, "low");
    }

    #[test]
    fn session_store_reload_builds_lazy_listing_index() {
        let (d, store) = temp_store("lazy-index");
        let instructions = vec![user_msg(&"instruction body ".repeat(10_000))];
        let sess = store.create("large-model", "/tmp", instructions);
        store.update(
            &sess.id,
            vec![user_msg(&format!(
                "recognizable title\n{}",
                "message body ".repeat(10_000)
            ))],
            "",
        );
        drop(store);

        let reload = SessionStore::open_in_dir(d.path()).unwrap();
        let infos = reload.list_info();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].id, sess.id);
        assert_eq!(infos[0].title, "recognizable title");
        assert_eq!(infos[0].model, "large-model");
        assert_eq!(infos[0].message_count, 1);

        // Listing remains available after startup without consulting the
        // transcript, while get still requires and lazily reads that file.
        std::fs::remove_file(d.path().join(format!("{}.json", sess.id))).unwrap();
        assert_eq!(reload.list_info().len(), 1);
        assert!(reload.get(&sess.id).is_none());
    }

    #[test]
    fn session_store_modify_then_update_preserves_modified_fields() {
        let (d, store) = temp_store("modify-update");
        let sess = store.create("m", "/tmp", vec![user_msg("instructions")]);
        let usage = StreamUsage {
            prompt_tokens: 123,
            completion_tokens: 45,
            total_tokens: 168,
            reasoning_tokens: 12,
            cost: 0.25,
            ..Default::default()
        };
        assert!(store.modify(&sess.id, |stored| {
            stored.usage = Some(usage.clone());
            stored.compaction_summary = "summary retained across update".into();
            stored.compacted_through = 7;
            stored.thinking = "high".into();
        }));
        store.update(&sess.id, vec![user_msg("new turn")], "fallback title");
        drop(store);

        let reload = SessionStore::open_in_dir(d.path()).unwrap();
        let got = reload.get(&sess.id).unwrap();
        assert_eq!(got.usage.as_ref().unwrap().prompt_tokens, 123);
        assert_eq!(got.usage.as_ref().unwrap().completion_tokens, 45);
        assert_eq!(got.usage.as_ref().unwrap().reasoning_tokens, 12);
        assert_eq!(got.usage.as_ref().unwrap().cost, 0.25);
        assert_eq!(got.compaction_summary, "summary retained across update");
        assert_eq!(got.compacted_through, 7);
        assert_eq!(got.thinking, "high");
        assert_eq!(got.messages.len(), 1);
        assert_eq!(got.messages[0].content, "new turn");
    }

    #[test]
    fn session_store_delete() {
        let (d, store) = temp_store("delete");
        let sess = store.create("m", "/tmp", vec![]);
        let path = d.path().join(format!("{}.json", sess.id));
        assert!(path.exists(), "session file missing after create");

        store.delete(&sess.id);
        assert!(
            store.get(&sess.id).is_none(),
            "get after delete should return None"
        );
        assert!(!path.exists(), "session file still exists");
        store.delete("unknown-id"); // must not panic
    }

    #[test]
    fn go_written_null_fields_parse() {
        // Go's encoding/json marshals nil slices as explicit null; the
        // Rust structs must treat those as zero values.
        let raw = r#"{
            "id": "abc123", "title": "t", "model": "m", "cwd": "/tmp",
            "messages": null, "instructions": null,
            "created_at": "2026-01-01T00:00:00Z", "updated_at": "2026-01-01T00:00:00Z"
        }"#;
        let s: Session = serde_json::from_str(raw).unwrap();
        assert!(s.messages.is_empty());
        let msg = r#"{"role":"assistant","content":"x","tool_calls":null}"#;
        let m: crate::types::Message = serde_json::from_str(msg).unwrap();
        assert!(m.tool_calls.is_empty());
    }

    #[test]
    fn session_files_stay_interchangeable_with_go() {
        let (d, store) = temp_store("json");
        let sess = store.create_child("parent123", "m", "/tmp", "low", "", vec![user_msg("x")]);
        store.update(
            &sess.id,
            vec![
                user_msg("q"),
                Message {
                    role: "assistant".into(),
                    content: "a".into(),
                    ..Default::default()
                },
            ],
            "",
        );
        store.update_title(&sess.id, "T");
        let raw = std::fs::read_to_string(d.path().join(format!("{}.json", sess.id))).unwrap();
        let v: serde_json::Value = serde_json::from_str(&raw).unwrap();
        // compacted_through is omitempty and zero here, so absent like Go.
        for key in [
            "id",
            "title",
            "title_generated",
            "messages",
            "model",
            "cwd",
            "instructions",
            "thinking",
            "created_at",
            "updated_at",
        ] {
            assert!(v.get(key).is_some(), "missing JSON key {key}");
        }
        assert!(v.get("parent_id").is_some());
        // Round-trips through the store loader.
        let reload = SessionStore::open_in_dir(d.path()).unwrap();
        let got = reload.get(&sess.id).unwrap();
        assert_eq!(got.title, "T");
        assert!(got.title_generated);
        assert_eq!(got.parent_id, "parent123");
    }

    #[test]
    fn first_line_trunc_trims_without_ellipsis() {
        assert_eq!(first_line_trunc("  hello \nworld", 60), "hello");
        assert_eq!(first_line_trunc("abcdefghij", 4), "abcd");
    }

    #[test]
    fn session_name_uses_first_user_line() {
        let msgs = vec![
            Message {
                role: "system".into(),
                content: "sys".into(),
                ..Default::default()
            },
            user_msg("fix the bug\nplease"),
        ];
        assert_eq!(session_name(&msgs), "fix the bug");
        let long: String = "x".repeat(80);
        assert_eq!(session_name(&[user_msg(&long)]).chars().count(), 60);
        assert_eq!(session_name(&[]), "");
    }

    #[test]
    fn file_seen_round_trip() {
        let (_d, store) = temp_store("fileseen");
        let sess = store.create("m", "/tmp", vec![]);
        assert!(store.seen_file(&sess.id, "/tmp/a.txt").is_none());
        store.remember_file(&sess.id, "/tmp/a.txt", b"hello");
        let e = store.seen_file(&sess.id, "/tmp/a.txt").unwrap();
        assert_eq!(e.data, b"hello".to_vec());
        assert_eq!(e.hash, sha256_hash(b"hello"));
        assert!(store.seen_file("missing", "/tmp/a.txt").is_none());
    }

    /// Replicates server.go persistSession's fallback-title rule so the
    /// store interplay can be tested without the server crate.
    fn persist_session_fallback_title(sess: &Session) -> String {
        if sess.title_generated {
            return String::new();
        }
        for m in &sess.messages {
            if m.role == "user" {
                let mut title = m.content.clone();
                if title.len() > 60 {
                    title = format!("{}...", &title[..60]);
                }
                return title;
            }
        }
        String::new()
    }

    #[test]
    fn persist_session_keeps_generated_title() {
        let (_d, store) = temp_store("persist-title");
        let sess = store.create("m", "/tmp", vec![]);
        let long_user: String = "please do this task now ".repeat(10);
        store.modify(&sess.id, |s| {
            s.messages = vec![user_msg(&long_user)];
        });

        // First persist: fallback title from the first user message.
        let snapshot = store.get(&sess.id).unwrap();
        let fallback = persist_session_fallback_title(&snapshot);
        store.update(&sess.id, snapshot.messages.clone(), &fallback);
        let got = store.get(&sess.id).unwrap();
        assert!(
            !got.title_generated,
            "first persist should not mark TitleGenerated"
        );
        assert!(!got.title.is_empty(), "fallback title should be set");
        assert!(
            got.title.len() >= 60,
            "fallback should truncate long first user message, got {:?}",
            got.title
        );

        // LLM names the session; later persists must not clobber it.
        store.update_title(&sess.id, "LLM session title");
        let mut got = store.get(&sess.id).unwrap();
        got.messages.push(Message {
            role: "assistant".into(),
            content: "ok".into(),
            ..Default::default()
        });
        let title = persist_session_fallback_title(&got);
        store.update(&got.id, got.messages.clone(), &title);

        let again = store.get(&sess.id).unwrap();
        assert_eq!(
            again.title, "LLM session title",
            "persist clobbered LLM title"
        );
        assert!(again.title_generated, "TitleGenerated should stay true");
        assert_eq!(again.messages.len(), 2);
    }
}
