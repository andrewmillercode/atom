//! Session store and persistence, ported from session.go.

use crate::types::{Message, StreamUsage};
use crate::util::{add_stream_usage, sha256_hash};
use chrono::{DateTime, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, RwLock};

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

/// atom data directory, honouring XDG_DATA_HOME
/// (~/.local/share/atom, or atom-dev for dev builds — see
/// atom_core::build).
pub fn data_dir() -> PathBuf {
    let base = std::env::var("XDG_DATA_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|h| h.join(".local/share")))
        .unwrap_or_else(|| std::env::temp_dir().join("atom"));
    let d = base.join(crate::build::dir_leaf());
    let _ = std::fs::create_dir_all(&d);
    d
}

/// Unix socket path for the session server.
pub fn socket_path() -> PathBuf {
    data_dir().join("atom.sock")
}

/// Directory where the session database lives.
pub fn sessions_dir() -> PathBuf {
    data_dir().join("sessions")
}

/// newSessionID: 16-char random hex.
pub fn new_session_id() -> String {
    use rand::Rng;
    let b: [u8; 8] = rand::thread_rng().gen();
    hex::encode(b)
}

/// SessionStore keeps a compact session index in memory. Full transcripts are
/// loaded from the normalized SQLite tables on demand.
pub struct SessionStore {
    dir: PathBuf,
    db: Mutex<Connection>,
    index: RwLock<HashMap<String, SessionInfo>>,
    mutation: Mutex<()>,
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
        let db = Connection::open(dir.join("sessions.sqlite3"))?;
        db.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;

             CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY,
                 title TEXT NOT NULL,
                 title_generated INTEGER NOT NULL,
                 model TEXT NOT NULL,
                 provider TEXT NOT NULL,
                 cwd TEXT NOT NULL,
                 usage TEXT,
                 compaction_summary TEXT NOT NULL,
                 compacted_through INTEGER NOT NULL,
                 parent_id TEXT NOT NULL,
                 thinking TEXT NOT NULL,
                 cancelled INTEGER NOT NULL,
                 status TEXT NOT NULL,
                 batch_id TEXT NOT NULL,
                 batch_index INTEGER NOT NULL,
                 created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL,
                 message_count INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS session_messages (
                 session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 position INTEGER NOT NULL,
                 message TEXT NOT NULL,
                 PRIMARY KEY (session_id, position)
             );
             CREATE TABLE IF NOT EXISTS session_instructions (
                 session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
                 position INTEGER NOT NULL,
                 message TEXT NOT NULL,
                 PRIMARY KEY (session_id, position)
             );
             CREATE INDEX IF NOT EXISTS sessions_parent_id_idx ON sessions(parent_id);
             CREATE INDEX IF NOT EXISTS sessions_updated_at_idx ON sessions(updated_at DESC);",
        )?;
        let s = SessionStore {
            dir,
            db: Mutex::new(db),
            index: RwLock::new(HashMap::new()),
            mutation: Mutex::new(()),
            file_seen: Mutex::new(HashMap::new()),
        };
        s.load_all()?;
        Ok(s)
    }

    pub fn dir(&self) -> &PathBuf {
        &self.dir
    }

    /// loadAll reads only scalar listing metadata. Transcript tables are not
    /// consulted during startup.
    fn load_all(&self) -> anyhow::Result<()> {
        let db = self.db.lock().unwrap();
        let mut stmt = db.prepare(
            "SELECT id, title, model, provider, message_count, usage, parent_id,
                    thinking, cancelled, status, batch_id, batch_index,
                    created_at, updated_at
             FROM sessions",
        )?;
        let rows = stmt.query_map([], session_info_from_row)?;
        let mut index = self.index.write().unwrap();
        for info in rows {
            let info = info?;
            index.insert(info.id.clone(), info);
        }
        Ok(())
    }

    /// Create makes a new session with the given model, cwd, and
    /// instructions, persists it, and returns it.
    pub fn create(&self, model: &str, cwd: &str, instructions: Vec<Message>) -> Session {
        let _mutation = self.mutation.lock().unwrap();
        let sess = Session {
            id: new_session_id(),
            model: model.into(),
            cwd: cwd.into(),
            instructions,
            ..Default::default()
        };
        self.save(&sess);
        self.index
            .write()
            .unwrap()
            .insert(sess.id.clone(), sess.info());
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
        let _mutation = self.mutation.lock().unwrap();
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
        self.save(&sess);
        self.index
            .write()
            .unwrap()
            .insert(sess.id.clone(), sess.info());
        sess
    }

    /// IsDescendantOf reports whether childID is parented (directly or
    /// nested) under ancestorID. A session is not a descendant of itself.
    pub fn is_descendant_of(&self, child_id: &str, ancestor_id: &str) -> bool {
        if child_id.is_empty() || ancestor_id.is_empty() || child_id == ancestor_id {
            return false;
        }
        let index = self.index.read().unwrap();
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
        let mut children: Vec<SessionInfo> = self
            .index
            .read()
            .unwrap()
            .values()
            .filter(|info| info.parent_id == parent_id)
            .cloned()
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
            .read()
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
        if !self.index.read().unwrap().contains_key(id) {
            return None;
        }
        self.load(id)
    }

    /// GetInfo returns listing metadata without loading the transcript.
    pub fn get_info(&self, id: &str) -> Option<SessionInfo> {
        self.index.read().unwrap().get(id).cloned()
    }

    /// List returns all sessions sorted by UpdatedAt descending (most
    /// recent first). Full sessions are loaded from disk for the caller but
    /// are not retained by the store.
    pub fn list(&self) -> Vec<Session> {
        let mut infos: Vec<SessionInfo> = self.index.read().unwrap().values().cloned().collect();
        infos.sort_by_key(|a| std::cmp::Reverse(a.updated_at));
        infos
            .into_iter()
            .filter_map(|info| self.load(&info.id))
            .collect()
    }

    /// ListInfo returns compact listing metadata without loading session
    /// transcripts, sorted by UpdatedAt descending.
    pub fn list_info(&self) -> Vec<SessionInfo> {
        let mut list: Vec<SessionInfo> = self.index.read().unwrap().values().cloned().collect();
        list.sort_by_key(|a| std::cmp::Reverse(a.updated_at));
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
            } else if sess.title.is_empty() {
                // Persist the fallback so future startup indexing never has
                // to inspect conversation content to recover a title.
                sess.title = session_name(&sess.messages);
            }
            sess.updated_at = Utc::now();
        });
    }

    /// Persists the fields owned by an in-flight turn in one transaction.
    /// Metadata that may change concurrently (provider, generated title,
    /// cancellation, and delegate state) remains sourced from the store.
    pub fn update_turn_snapshot(&self, id: &str, snapshot: &Session, title: &str) -> bool {
        self.mutate(id, |sess| {
            sess.messages = snapshot.messages.clone();
            sess.usage = snapshot.usage.clone();
            sess.compaction_summary = snapshot.compaction_summary.clone();
            sess.compacted_through = snapshot.compacted_through;
            sess.thinking = snapshot.thinking.clone();
            if !title.is_empty() && !sess.title_generated {
                sess.title = title.to_string();
            } else if sess.title.is_empty() {
                sess.title = session_name(&sess.messages);
            }
            sess.updated_at = Utc::now();
        })
    }

    /// UpdateTitle sets Title, marks TitleGenerated, and saves. Messages
    /// are left unchanged.
    pub fn update_title(&self, id: &str, title: &str) {
        let _mutation = self.mutation.lock().unwrap();
        if !self.index.read().unwrap().contains_key(id) {
            return;
        }
        let now = Utc::now();
        if self
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE sessions SET title = ?1, title_generated = 1, updated_at = ?2 WHERE id = ?3",
                params![title, now.to_rfc3339(), id],
            )
            .is_ok()
        {
            if let Some(info) = self.index.write().unwrap().get_mut(id) {
                info.title = title.to_string();
                info.updated_at = now;
            }
        }
    }

    /// UpdateModel changes the model that answers future messages in a
    /// session. The conversation history is kept; only the model used
    /// for new turns changes. It's a no-op if the session doesn't exist.
    pub fn update_model(&self, id: &str, model: &str) {
        let _mutation = self.mutation.lock().unwrap();
        if !self.index.read().unwrap().contains_key(id) {
            return;
        }
        let now = Utc::now();
        if self
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE sessions SET model = ?1, updated_at = ?2 WHERE id = ?3",
                params![model, now.to_rfc3339(), id],
            )
            .is_ok()
        {
            if let Some(info) = self.index.write().unwrap().get_mut(id) {
                info.model = model.to_string();
                info.updated_at = now;
            }
        }
    }

    /// UpdateProvider records the backend selected for a dispatched child.
    pub fn update_provider(&self, id: &str, provider: &str) {
        let _mutation = self.mutation.lock().unwrap();
        if !self.index.read().unwrap().contains_key(id) {
            return;
        }
        let now = Utc::now();
        if self
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE sessions SET provider = ?1, updated_at = ?2 WHERE id = ?3",
                params![provider, now.to_rfc3339(), id],
            )
            .is_ok()
        {
            if let Some(info) = self.index.write().unwrap().get_mut(id) {
                info.provider = provider.to_string();
                info.updated_at = now;
            }
        }
    }

    /// UpdateThinking stores the TUI's current reasoning_effort for this
    /// session so Tab/Ctrl+T cycles survive reloads after a turn saves.
    pub fn update_thinking(&self, id: &str, thinking: &str) {
        let _mutation = self.mutation.lock().unwrap();
        if !self.index.read().unwrap().contains_key(id) {
            return;
        }
        let now = Utc::now();
        if self
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE sessions SET thinking = ?1, updated_at = ?2 WHERE id = ?3",
                params![thinking, now.to_rfc3339(), id],
            )
            .is_ok()
        {
            if let Some(info) = self.index.write().unwrap().get_mut(id) {
                info.thinking = thinking.to_string();
                info.updated_at = now;
            }
        }
    }

    /// SetCancelled records whether a dispatched subagent was explicitly
    /// killed by its parent (true) or revived by a follow-up (false).
    pub fn set_cancelled(&self, id: &str, cancelled: bool) -> bool {
        let _mutation = self.mutation.lock().unwrap();
        if !self.index.read().unwrap().contains_key(id) {
            return false;
        }
        let now = Utc::now();
        if self
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE sessions SET cancelled = ?1, updated_at = ?2 WHERE id = ?3",
                params![cancelled, now.to_rfc3339(), id],
            )
            .is_err()
        {
            return false;
        }
        if let Some(info) = self.index.write().unwrap().get_mut(id) {
            info.cancelled = cancelled;
            info.updated_at = now;
        }
        true
    }

    pub fn update_delegate_status(&self, id: &str, status: DelegateStatus) -> bool {
        let _mutation = self.mutation.lock().unwrap();
        if !self.index.read().unwrap().contains_key(id) {
            return false;
        }
        let now = Utc::now();
        if self
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE sessions SET status = ?1, updated_at = ?2 WHERE id = ?3",
                params![status.as_str(), now.to_rfc3339(), id],
            )
            .is_err()
        {
            return false;
        }
        if let Some(info) = self.index.write().unwrap().get_mut(id) {
            info.status = status;
            info.updated_at = now;
        }
        true
    }

    pub fn update_delegate_batch(&self, id: &str, batch_id: &str, batch_index: usize) -> bool {
        let _mutation = self.mutation.lock().unwrap();
        if !self.index.read().unwrap().contains_key(id) {
            return false;
        }
        let now = Utc::now();
        if self
            .db
            .lock()
            .unwrap()
            .execute(
                "UPDATE sessions SET batch_id = ?1, batch_index = ?2, updated_at = ?3 WHERE id = ?4",
                params![batch_id, batch_index as i64, now.to_rfc3339(), id],
            )
            .is_err()
        {
            return false;
        }
        if let Some(info) = self.index.write().unwrap().get_mut(id) {
            info.batch_id = batch_id.to_string();
            info.batch_index = batch_index as i64;
            info.updated_at = now;
        }
        true
    }

    /// Turns left active by a daemon exit cannot still be running after a
    /// fresh server starts. Mark them as errors instead of showing a stuck
    /// spinner forever.
    pub fn reconcile_delegate_statuses(&self) {
        let ids: Vec<String> = self
            .index
            .read()
            .unwrap()
            .values()
            .filter(|info| !info.parent_id.is_empty() && info.status.is_active())
            .map(|info| info.id.clone())
            .collect();
        for id in ids {
            self.update_delegate_status(&id, DelegateStatus::Error);
        }
    }

    /// Delete removes a session and its related rows.
    pub fn delete(&self, id: &str) {
        let _mutation = self.mutation.lock().unwrap();
        let mut db = self.db.lock().unwrap();
        if let Ok(tx) = db.transaction() {
            if tx
                .execute("DELETE FROM sessions WHERE id = ?1", [id])
                .is_ok()
            {
                let _ = tx.commit();
            }
        };
        drop(db);
        self.index.write().unwrap().remove(id);
        self.file_seen.lock().unwrap().remove(id);
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

    /// load reads a full session and its ordered child rows. Callers hold the
    /// index lock so a concurrent mutation cannot interleave with the read.
    fn load(&self, id: &str) -> Option<Session> {
        self.load_result(id).ok().flatten()
    }

    fn load_result(&self, id: &str) -> anyhow::Result<Option<Session>> {
        let db = self.db.lock().unwrap();
        let sess = db
            .query_row(
                "SELECT id, title, title_generated, model, provider, cwd, usage,
                        compaction_summary, compacted_through, parent_id, thinking,
                        cancelled, status, batch_id, batch_index, created_at, updated_at
                 FROM sessions WHERE id = ?1",
                [id],
                session_from_row,
            )
            .optional()?;
        let Some(mut sess) = sess else {
            return Ok(None);
        };
        sess.messages = load_message_rows(
            &db,
            "SELECT message FROM session_messages WHERE session_id = ?1 ORDER BY position",
            id,
        )?;
        sess.instructions = load_message_rows(
            &db,
            "SELECT message FROM session_instructions WHERE session_id = ?1 ORDER BY position",
            id,
        )?;
        Ok(Some(sess))
    }

    /// mutate serializes a full-session read/modify/write with all other
    /// session operations and refreshes the compact index.
    fn mutate<F: FnOnce(&mut Session)>(&self, id: &str, f: F) -> bool {
        let _mutation = self.mutation.lock().unwrap();
        if !self.index.read().unwrap().contains_key(id) {
            return false;
        }
        let Some(mut sess) = self.load(id) else {
            return false;
        };
        f(&mut sess);
        if self.save_result(&sess).is_err() {
            return false;
        }
        self.index
            .write()
            .unwrap()
            .insert(id.to_string(), sess.info());
        true
    }

    /// save replaces all persisted session state atomically. Callers hold the
    /// index lock to keep SQLite and metadata updates ordered.
    fn save(&self, sess: &Session) {
        let _ = self.save_result(sess);
    }

    fn save_result(&self, sess: &Session) -> anyhow::Result<()> {
        let usage = sess.usage.as_ref().map(serde_json::to_string).transpose()?;
        let messages: Vec<String> = sess
            .messages
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<_, _>>()?;
        let instructions: Vec<String> = sess
            .instructions
            .iter()
            .map(serde_json::to_string)
            .collect::<Result<_, _>>()?;
        let mut db = self.db.lock().unwrap();
        let tx = db.transaction()?;
        tx.execute(
            "INSERT INTO sessions (
                 id, title, title_generated, model, provider, cwd, usage,
                 compaction_summary, compacted_through, parent_id, thinking,
                 cancelled, status, batch_id, batch_index, created_at, updated_at,
                 message_count
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                 ?14, ?15, ?16, ?17, ?18
             ) ON CONFLICT(id) DO UPDATE SET
                 title=excluded.title, title_generated=excluded.title_generated,
                 model=excluded.model, provider=excluded.provider, cwd=excluded.cwd,
                 usage=excluded.usage, compaction_summary=excluded.compaction_summary,
                 compacted_through=excluded.compacted_through,
                 parent_id=excluded.parent_id, thinking=excluded.thinking,
                 cancelled=excluded.cancelled, status=excluded.status,
                 batch_id=excluded.batch_id, batch_index=excluded.batch_index,
                 created_at=excluded.created_at, updated_at=excluded.updated_at,
                 message_count=excluded.message_count",
            params![
                sess.id,
                sess.title,
                sess.title_generated,
                sess.model,
                sess.provider,
                sess.cwd,
                usage,
                sess.compaction_summary,
                sess.compacted_through,
                sess.parent_id,
                sess.thinking,
                sess.cancelled,
                sess.status.as_str(),
                sess.batch_id,
                sess.batch_index,
                sess.created_at.to_rfc3339(),
                sess.updated_at.to_rfc3339(),
                sess.messages.len() as i64,
            ],
        )?;
        tx.execute(
            "DELETE FROM session_messages WHERE session_id = ?1",
            [&sess.id],
        )?;
        tx.execute(
            "DELETE FROM session_instructions WHERE session_id = ?1",
            [&sess.id],
        )?;
        {
            let mut statement = tx.prepare_cached(
                "INSERT INTO session_messages (session_id, position, message) VALUES (?1, ?2, ?3)",
            )?;
            for (position, message) in messages.iter().enumerate() {
                statement.execute(params![sess.id, position as i64, message])?;
            }
        }
        {
            let mut statement = tx.prepare_cached(
                "INSERT INTO session_instructions (session_id, position, message) VALUES (?1, ?2, ?3)",
            )?;
            for (position, message) in instructions.iter().enumerate() {
                statement.execute(params![sess.id, position as i64, message])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}

fn session_info_from_row(row: &Row<'_>) -> rusqlite::Result<SessionInfo> {
    Ok(SessionInfo {
        id: row.get(0)?,
        title: row.get(1)?,
        model: row.get(2)?,
        provider: row.get(3)?,
        message_count: row.get::<_, i64>(4)? as usize,
        usage: usage_from_json(row.get(5)?)?,
        parent_id: row.get(6)?,
        thinking: row.get(7)?,
        cancelled: row.get(8)?,
        status: status_from_str(&row.get::<_, String>(9)?)?,
        batch_id: row.get(10)?,
        batch_index: row.get(11)?,
        created_at: datetime_from_str(&row.get::<_, String>(12)?)?,
        updated_at: datetime_from_str(&row.get::<_, String>(13)?)?,
    })
}

fn session_from_row(row: &Row<'_>) -> rusqlite::Result<Session> {
    Ok(Session {
        id: row.get(0)?,
        title: row.get(1)?,
        title_generated: row.get(2)?,
        messages: Vec::new(),
        model: row.get(3)?,
        provider: row.get(4)?,
        cwd: row.get(5)?,
        instructions: Vec::new(),
        usage: usage_from_json(row.get(6)?)?,
        compaction_summary: row.get(7)?,
        compacted_through: row.get(8)?,
        parent_id: row.get(9)?,
        thinking: row.get(10)?,
        cancelled: row.get(11)?,
        status: status_from_str(&row.get::<_, String>(12)?)?,
        batch_id: row.get(13)?,
        batch_index: row.get(14)?,
        created_at: datetime_from_str(&row.get::<_, String>(15)?)?,
        updated_at: datetime_from_str(&row.get::<_, String>(16)?)?,
    })
}

fn load_message_rows(db: &Connection, sql: &str, id: &str) -> anyhow::Result<Vec<Message>> {
    let mut stmt = db.prepare(sql)?;
    let rows = stmt.query_map([id], |row| row.get::<_, String>(0))?;
    let mut messages = Vec::new();
    for row in rows {
        messages.push(message_from_json(&row?)?);
    }
    Ok(messages)
}

fn message_from_json(json: &str) -> serde_json::Result<Message> {
    let mut value: serde_json::Value = serde_json::from_str(json)?;
    normalize_persisted_usage(value.get_mut("usage"));
    serde_json::from_value(value)
}

fn usage_from_json(json: Option<String>) -> rusqlite::Result<Option<StreamUsage>> {
    json.map(|json| {
        let mut value: serde_json::Value = serde_json::from_str(&json).map_err(sql_json_error)?;
        normalize_persisted_usage(Some(&mut value));
        serde_json::from_value(value).map_err(sql_json_error)
    })
    .transpose()
}

fn datetime_from_str(value: &str) -> rusqlite::Result<DateTime<Utc>> {
    value.parse::<DateTime<Utc>>().map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
    })
}

fn status_from_str(value: &str) -> rusqlite::Result<DelegateStatus> {
    match value {
        "queued" => Ok(DelegateStatus::Queued),
        "working" => Ok(DelegateStatus::Working),
        "sandbox" => Ok(DelegateStatus::Sandbox),
        "error" => Ok(DelegateStatus::Error),
        "done" => Ok(DelegateStatus::Done),
        "cancelled" => Ok(DelegateStatus::Cancelled),
        _ => Err(rusqlite::Error::FromSqlConversionFailure(
            0,
            rusqlite::types::Type::Text,
            format!("invalid delegate status: {value}").into(),
        )),
    }
}

fn sql_json_error(error: serde_json::Error) -> rusqlite::Error {
    rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(error))
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

        let db = Connection::open(d.path().join("sessions.sqlite3")).unwrap();
        db.execute(
            "UPDATE session_messages SET message = 'not json' WHERE session_id = ?1",
            [&sess.id],
        )
        .unwrap();
        drop(db);

        let reload = SessionStore::open_in_dir(d.path()).unwrap();
        let infos = reload.list_info();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].id, sess.id);
        assert_eq!(infos[0].title, "recognizable title");
        assert_eq!(infos[0].model, "large-model");
        assert_eq!(infos[0].message_count, 1);
        // Startup and metadata listing never deserialize transcript rows.
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
    fn metadata_listing_does_not_wait_for_transcript_mutation() {
        let (_d, store) = temp_store("metadata-during-mutation");
        let store = std::sync::Arc::new(store);
        let sess = store.create("m", "/tmp", vec![]);
        let id = sess.id.clone();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let writer_store = store.clone();
        let writer = std::thread::spawn(move || {
            writer_store.modify(&id, |_| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
        });
        entered_rx.recv().unwrap();

        let (listed_tx, listed_rx) = std::sync::mpsc::channel();
        let list_store = store.clone();
        let reader = std::thread::spawn(move || {
            listed_tx.send(list_store.list_info()).unwrap();
        });
        let listed = listed_rx.recv_timeout(Duration::from_millis(250));
        release_tx.send(()).unwrap();
        assert!(writer.join().unwrap());
        reader.join().unwrap();

        let listed = listed.expect("metadata listing blocked behind transcript work");
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, sess.id);
    }

    #[test]
    fn turn_snapshot_preserves_concurrent_metadata() {
        let (_d, store) = temp_store("turn-snapshot");
        let child = store.create_child("parent", "m", "/tmp", "low", "fallback", vec![]);
        store.update_title(&child.id, "Generated title");
        store.update_delegate_status(&child.id, DelegateStatus::Working);

        let mut snapshot = store.get(&child.id).unwrap();
        snapshot.messages = vec![user_msg("new transcript")];
        snapshot.compaction_summary = "summary".into();
        snapshot.compacted_through = 3;
        snapshot.thinking = "high".into();
        assert!(store.update_turn_snapshot(&child.id, &snapshot, "new fallback"));

        let got = store.get(&child.id).unwrap();
        assert_eq!(got.title, "Generated title");
        assert!(got.title_generated);
        assert_eq!(got.status, DelegateStatus::Working);
        assert_eq!(got.messages[0].content, "new transcript");
        assert_eq!(got.compaction_summary, "summary");
        assert_eq!(got.compacted_through, 3);
        assert_eq!(got.thinking, "high");
    }

    #[test]
    fn session_store_delete() {
        let (d, store) = temp_store("delete");
        let sess = store.create("m", "/tmp", vec![user_msg("instruction")]);
        store.update(&sess.id, vec![user_msg("message")], "title");

        store.delete(&sess.id);
        assert!(
            store.get(&sess.id).is_none(),
            "get after delete should return None"
        );
        let db = Connection::open(d.path().join("sessions.sqlite3")).unwrap();
        for table in ["sessions", "session_messages", "session_instructions"] {
            let count: i64 = db
                .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get(0)
                })
                .unwrap();
            assert_eq!(count, 0, "rows remain in {table}");
        }
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
    fn session_store_uses_normalized_rows_only() {
        let (d, store) = temp_store("normalized");
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
        drop(store);

        assert!(!d.path().join(format!("{}.json", sess.id)).exists());
        let db = Connection::open(d.path().join("sessions.sqlite3")).unwrap();
        let counts: (i64, i64, i64) = (
            db.query_row("SELECT count(*) FROM sessions", [], |row| row.get(0))
                .unwrap(),
            db.query_row("SELECT count(*) FROM session_messages", [], |row| {
                row.get(0)
            })
            .unwrap(),
            db.query_row("SELECT count(*) FROM session_instructions", [], |row| {
                row.get(0)
            })
            .unwrap(),
        );
        assert_eq!(counts, (1, 2, 1));
        drop(db);

        let reload = SessionStore::open_in_dir(d.path()).unwrap();
        let got = reload.get(&sess.id).unwrap();
        assert_eq!(got.title, "T");
        assert!(got.title_generated);
        assert_eq!(got.parent_id, "parent123");
    }

    #[test]
    fn session_store_ignores_json_files() {
        let (d, store) = temp_store("ignore-json");
        std::fs::write(d.path().join("legacy.json"), r#"{"id":"legacy"}"#).unwrap();
        drop(store);

        let reload = SessionStore::open_in_dir(d.path()).unwrap();
        assert!(reload.list_info().is_empty());
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
