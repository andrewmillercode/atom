//! ACP (Agent Client Protocol) client support: atom acts as the ACP
//! client for external coding agents (claude-code-acp, Gemini CLI, Codex
//! CLI, …) spawned as subprocesses speaking JSON-RPC 2.0 over stdio.
//!
//! Wire types are hand-rolled (same approach as mcp.rs) covering the
//! spec's client-side surface: initialize, session/new / load / prompt /
//! cancel / set_mode, the session/update notification family,
//! session/request_permission, and the fs read/write callbacks. Terminal
//! and elicitation capabilities are advertised off.

use once_cell::sync::Lazy;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

/// The protocol version atom speaks; agents may respond with any version
/// ≤ this (spec: client supports a range, agent replies with one).
pub const PROTOCOL_VERSION: u64 = 1;
/// Provider name sessions carry when they are driven by an ACP agent
/// (the agent's name lives in the session model field).
pub const ACP_PROVIDER_NAME: &str = "acp-agent";

const INITIALIZE_TIMEOUT: Duration = Duration::from_secs(60);
const CALL_TIMEOUT: Duration = Duration::from_secs(5 * 60);

// ---------------------------------------------------------------------------
// Config discovery (mcp.json-style).
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct AcpAgentConfig {
    #[serde(default)]
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub disabled: bool,
    /// Optional CLI that prints the agent's full model catalog as JSON
    /// (e.g. devin ships `devin models list --format json`). Devin's ACP
    /// surface advertises only the account's current model, so the picker
    /// merges this in to show everything.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models_command: Vec<String>,
}

/// acpAgents from `~/.config/atom/acp.json`, overridden by the nearest
/// project `.atom/acp.json` files (same precedence as mcp.json).
pub fn load_acp_configs(cwd: &str) -> BTreeMap<String, AcpAgentConfig> {
    let config = crate::skills::atom_config_dir();
    let home = dirs::home_dir();
    load_acp_configs_in(cwd, config.as_deref(), home.as_deref())
}

pub fn load_acp_configs_in(
    cwd: &str,
    config_dir: Option<&Path>,
    home: Option<&Path>,
) -> BTreeMap<String, AcpAgentConfig> {
    let mut out = BTreeMap::new();
    if let Some(dir) = config_dir {
        merge_acp_file(&mut out, &dir.join("acp.json"));
    }
    let dirs = crate::skills::walk_project_dirs_in(cwd, home);
    for d in dirs.iter().rev() {
        merge_acp_file(&mut out, &d.join(".atom").join("acp.json"));
    }
    out
}

fn merge_acp_file(out: &mut BTreeMap<String, AcpAgentConfig>, path: &Path) {
    let b = match std::fs::read(path) {
        Ok(b) => b,
        Err(_) => return,
    };
    #[derive(Deserialize)]
    struct File {
        #[serde(rename = "acpAgents", default)]
        acpagents: BTreeMap<String, serde_json::Value>,
    }
    let Ok(file) = serde_json::from_slice::<File>(&b) else {
        return;
    };
    for (name, raw) in file.acpagents {
        let Ok(mut cfg) = serde_json::from_value::<AcpAgentConfig>(raw) else {
            continue;
        };
        for arg in cfg.args.iter_mut().filter(|a| *a == CLAUDE_ACP_DEPRECATED) {
            *arg = CLAUDE_ACP_PACKAGE.into();
        }
        if cfg.disabled {
            out.remove(&name);
            continue;
        }
        out.insert(name, cfg);
    }
}

const CLAUDE_ACP_PACKAGE: &str = "@agentclientprotocol/claude-agent-acp";
const CLAUDE_ACP_DEPRECATED: &str = "@zed-industries/claude-code-acp";

/// Zed-style agents offered for one-key setup in /providers: (id,
/// display label, default launch). Enter on an unconfigured row writes
/// this launch into the user's acp.json.
pub fn bundled_acp_agents() -> Vec<(&'static str, &'static str, AcpAgentConfig)> {
    vec![
        (
            "claude-code",
            "Claude Code",
            AcpAgentConfig {
                command: "npx".into(),
                args: vec!["-y".into(), CLAUDE_ACP_PACKAGE.into()],
                ..Default::default()
            },
        ),
        (
            "codex",
            "Codex",
            AcpAgentConfig {
                command: "npx".into(),
                args: vec!["-y".into(), "@agentclientprotocol/codex-acp".into()],
                ..Default::default()
            },
        ),
        (
            "gemini",
            "Gemini CLI",
            AcpAgentConfig {
                command: "gemini".into(),
                args: vec!["--experimental-acp".into()],
                ..Default::default()
            },
        ),
        (
            "devin",
            "Devin",
            AcpAgentConfig {
                command: "devin".into(),
                args: vec!["acp".into()],
                models_command: vec![
                    "devin".into(),
                    "models".into(),
                    "list".into(),
                    "--format".into(),
                    "json".into(),
                ],
                ..Default::default()
            },
        ),
    ]
}

/// The user-level acp.json (the file one-key setup writes).
pub fn user_acp_config_path() -> Option<PathBuf> {
    crate::skills::atom_config_dir().map(|d| d.join("acp.json"))
}

/// add_agent merges one agent into the user-level acp.json, preserving
/// the file's other content. Creates the file when missing.
pub fn add_agent(name: &str, cfg: &AcpAgentConfig) -> Result<(), String> {
    let path = user_acp_config_path().ok_or("no atom config dir")?;
    let mut root = match std::fs::read(&path) {
        Ok(b) => serde_json::from_slice::<Value>(&b)
            .map_err(|e| format!("parse {}: {e}", path.display()))?,
        Err(_) => serde_json::json!({"acpAgents": {}}),
    };
    let agents = root
        .as_object_mut()
        .ok_or("acp.json root is not an object")?
        .entry("acpAgents")
        .or_insert_with(|| json!({}));
    agents
        .as_object_mut()
        .ok_or("acpAgents is not an object")?
        .insert(
            name.to_string(),
            serde_json::to_value(cfg).unwrap_or_default(),
        );
    write_acp_file(&path, &root)
}

/// remove_agent deletes one agent from the user-level acp.json. Returns
/// false when the name wasn't there.
pub fn remove_agent(name: &str) -> Result<bool, String> {
    let Some(path) = user_acp_config_path() else {
        return Ok(false);
    };
    let Ok(b) = std::fs::read(&path) else {
        return Ok(false);
    };
    let mut root: Value = serde_json::from_slice(&b).map_err(|e| format!("parse {e}"))?;
    let removed = root
        .as_object_mut()
        .and_then(|o| o.get_mut("acpAgents"))
        .and_then(|a| a.as_object_mut())
        .map(|agents| agents.remove(name).is_some())
        .unwrap_or(false);
    if removed {
        write_acp_file(&path, &root)?;
    }
    Ok(removed)
}

fn write_acp_file(path: &Path, root: &Value) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let bytes = serde_json::to_vec_pretty(root).map_err(|e| e.to_string())?;
    std::fs::write(path, bytes).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// Wire types (the subset the client side consumes or produces).
// ---------------------------------------------------------------------------

/// One ACP content block. Typed loosely on purpose: agents add new
/// block types and extra fields, and unknown shapes must not kill a
/// turn — unknown fields are dropped by serde, unknown `type`s degrade
/// to a name/description rendering downstream.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub typ: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub mime_type: String,
    #[serde(default)]
    pub data: String,
    #[serde(default)]
    pub uri: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub title: String,
}

impl ContentBlock {
    pub fn text(text: impl Into<String>) -> Self {
        ContentBlock {
            typ: "text".into(),
            text: text.into(),
            ..Default::default()
        }
    }

    /// Human summary used for tool result text and fallback rendering.
    pub fn describe(&self) -> String {
        match self.typ.as_str() {
            "text" => self.text.clone(),
            "image" => format!("[image {} {} bytes]", self.mime_type, self.data.len()),
            "resource_link" => format!("[link {} {}]", self.title, self.uri),
            "resource" => format!("[resource {}]", self.uri),
            "audio" => format!("[audio {}]", self.mime_type),
            other => format!("[{other}]"),
        }
    }

    /// Blocks carried by one `content` field: either a single block
    /// object or an array of them.
    pub fn list_from(v: &Value) -> Vec<ContentBlock> {
        match v {
            Value::Array(items) => items
                .iter()
                .filter_map(|b| serde_json::from_value(b.clone()).ok())
                .collect(),
            v if !v.is_null() => serde_json::from_value(v.clone()).ok().into_iter().collect(),
            _ => Vec::new(),
        }
    }
}

/// Embedded content of a tool call: a diff against a file, or a content
/// block (e.g. terminal output).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolCallContent {
    #[serde(rename = "type")]
    pub typ: String,
    #[serde(default)]
    pub path: String,
    #[serde(default)]
    pub old_text: Option<String>,
    #[serde(default)]
    pub new_text: Option<String>,
    #[serde(default)]
    pub content: Option<ContentBlock>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PlanEntry {
    pub content: String,
    pub priority: String,
    pub status: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AvailableCommand {
    pub name: String,
    pub description: String,
}

/// The demuxed shape of a `session/update` notification's `update` field.
#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    UserMessageChunk {
        content: Vec<ContentBlock>,
    },
    AgentMessageChunk {
        content: Vec<ContentBlock>,
    },
    AgentThoughtChunk {
        content: Vec<ContentBlock>,
    },
    ToolCall {
        id: String,
        title: String,
        kind: String,
        status: String,
        contents: Vec<ToolCallContent>,
        raw_input: Option<Value>,
    },
    ToolCallUpdate {
        id: String,
        title: String,
        kind: String,
        status: String,
        contents: Vec<ToolCallContent>,
        raw_input: Option<Value>,
        raw_output: Option<Value>,
    },
    UsageUpdate {
        used: i64,
        size: i64,
        cost: f64,
    },
    Plan {
        entries: Vec<PlanEntry>,
    },
    AvailableCommands {
        commands: Vec<AvailableCommand>,
    },
    CurrentMode {
        mode_id: String,
    },
    ConfigOptionUpdate {
        options: Vec<ConfigOption>,
    },
}

fn jstr(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// parseUpdate decodes the raw `update` object; unknown
/// `sessionUpdate` discriminators return None (skip, don't fail).
pub fn parse_update(v: &Value) -> Option<Update> {
    let update = match jstr(v, "sessionUpdate").as_str() {
        "user_message_chunk" => Update::UserMessageChunk {
            content: v
                .get("content")
                .map(ContentBlock::list_from)
                .unwrap_or_default(),
        },
        "agent_message_chunk" => Update::AgentMessageChunk {
            content: v
                .get("content")
                .map(ContentBlock::list_from)
                .unwrap_or_default(),
        },
        "agent_thought_chunk" => Update::AgentThoughtChunk {
            content: v
                .get("content")
                .map(ContentBlock::list_from)
                .unwrap_or_default(),
        },
        "tool_call" => Update::ToolCall {
            id: jstr(v, "toolCallId"),
            title: jstr(v, "title"),
            kind: jstr(v, "kind"),
            status: jstr(v, "status"),
            contents: v
                .get("content")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|c| serde_json::from_value(c.clone()).ok())
                        .collect()
                })
                .unwrap_or_default(),
            raw_input: v.get("rawInput").filter(|v| !v.is_null()).cloned(),
        },
        "tool_call_update" => Update::ToolCallUpdate {
            id: jstr(v, "toolCallId"),
            title: jstr(v, "title"),
            kind: jstr(v, "kind"),
            status: jstr(v, "status"),
            contents: v
                .get("content")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .filter_map(|c| serde_json::from_value(c.clone()).ok())
                        .collect()
                })
                .unwrap_or_default(),
            raw_input: v.get("rawInput").filter(|v| !v.is_null()).cloned(),
            raw_output: v.get("rawOutput").filter(|v| !v.is_null()).cloned(),
        },
        "usage_update" => Update::UsageUpdate {
            used: v.get("used").and_then(Value::as_i64).unwrap_or(0),
            size: v.get("size").and_then(Value::as_i64).unwrap_or(0),
            cost: v
                .get("cost")
                .filter(|c| jstr(c, "currency") == "USD")
                .and_then(|c| c.get("amount"))
                .and_then(Value::as_f64)
                .unwrap_or(0.0),
        },
        "plan" => Update::Plan {
            entries: v
                .get("entries")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .map(|e| PlanEntry {
                            content: jstr(e, "content"),
                            priority: jstr(e, "priority"),
                            status: jstr(e, "status"),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        },
        "available_commands_update" => Update::AvailableCommands {
            commands: v
                .get("availableCommands")
                .and_then(Value::as_array)
                .map(|items| {
                    items
                        .iter()
                        .map(|c| AvailableCommand {
                            name: jstr(c, "name"),
                            description: jstr(c, "description"),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        },
        "current_mode_update" => Update::CurrentMode {
            mode_id: jstr(v, "currentModeId"),
        },
        "config_option_update" => Update::ConfigOptionUpdate {
            options: v
                .get("configOptions")
                .map(parse_config_options)
                .unwrap_or_default(),
        },
        _ => return None,
    };
    Some(update)
}

/// Agent capabilities from the initialize response.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AgentCapabilities {
    #[serde(default)]
    pub load_session: bool,
    #[serde(default)]
    pub modes: Option<Value>,
    #[serde(default)]
    pub prompt_capabilities: Value,
    #[serde(default)]
    pub auth_methods: Vec<Value>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InitializeResult {
    #[serde(default)]
    pub protocol_version: u64,
    #[serde(default)]
    pub agent_capabilities: AgentCapabilities,
    #[serde(default)]
    pub auth_methods: Vec<Value>,
}

/// A session's advertised operating modes, if any.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct AgentModes {
    #[serde(rename = "currentModeId")]
    pub current_mode_id: String,
    #[serde(rename = "availableModes")]
    pub available: Vec<(String, String)>, // (id, name)
}

/// One session config option (the modern replacement for modes):
/// agents advertise selectors for model, thought level, mode, etc. on
/// session/new, changeable via session/set_config_option.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOption {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub category: String,
    #[serde(rename = "type", default)]
    pub typ: String,
    #[serde(default)]
    pub current_value: Value,
    #[serde(default)]
    pub options: Vec<ConfigOptionValue>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigOptionValue {
    pub value: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// parse_config_options decodes a configOptions array (from the
/// session/new result, a set_config_option response, or a
/// config_option_update notification).
pub fn parse_config_options(v: &Value) -> Vec<ConfigOption> {
    v.as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|o| serde_json::from_value(o.clone()).ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The first option whose category matches (e.g. "model").
pub fn config_option_for_category(opts: &[ConfigOption], category: &str) -> Option<ConfigOption> {
    opts.iter().find(|o| o.category == category).cloned()
}

/// One selectable model from the spec's `models` object or a catalog CLI.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    #[serde(default)]
    pub model_id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// The `models` object some agents return on session/new (Claude Code's
/// adapter does); selections ride session/set_model.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionModels {
    #[serde(rename = "currentModelId")]
    pub current_model_id: String,
    #[serde(rename = "availableModels")]
    pub available: Vec<ModelInfo>,
}

pub fn parse_session_models(v: &Value) -> Option<SessionModels> {
    let available = v.get("availableModels")?.as_array()?;
    if available.is_empty() {
        return None;
    }
    let models: Vec<ModelInfo> = available
        .iter()
        .filter_map(|m| serde_json::from_value(m.clone()).ok())
        .collect();
    if models.is_empty() {
        return None;
    }
    Some(SessionModels {
        current_model_id: jstr(v, "currentModelId"),
        available: models,
    })
}

/// merge_config_options overlays `incoming` (notifications and response
/// refreshes) onto `base` by option id: newer entries win wholesale.
/// Devin's mode list, for one, differs between the session/new result
/// and the config_option_update that races it.
pub fn merge_config_options(
    base: Vec<ConfigOption>,
    incoming: Vec<ConfigOption>,
) -> Vec<ConfigOption> {
    let mut out: Vec<String> = base.iter().map(|o| o.id.clone()).collect();
    let mut merged = base;
    for o in incoming {
        if let Some(i) = merged.iter().position(|existing| existing.id == o.id) {
            out.retain(|id| id != &merged[i].id);
            out.push(o.id.clone());
            merged[i] = o;
        } else {
            out.push(o.id.clone());
            merged.push(o);
        }
    }
    // Re-order by first appearance so the merged list keeps base order
    // for existing ids and appends new ones.
    let mut ordered = Vec::with_capacity(merged.len());
    let mut seen = Vec::new();
    for id in out {
        if seen.contains(&id) {
            continue;
        }
        seen.push(id.clone());
        if let Some(o) = merged.iter().find(|o| o.id == id) {
            ordered.push(o.clone());
        }
    }
    ordered
}

/// One row of a model catalog (from an agent's models_command CLI).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CatalogEntry {
    pub value: String,
    pub name: String,
    pub description: String,
}

/// parse_model_catalog reads a models_command's JSON output. Devin's
/// shape is {"families": [{slug, family_label, variants: [{model_uid,
/// label, description, …}]}]}; the generic shape is a flat array of
/// {value, name}. Unknown shapes yield an empty catalog, never an error
/// — the picker just shows fewer rows.
pub fn parse_model_catalog(raw: &str) -> Vec<CatalogEntry> {
    let v: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };
    let variants = |variant: &Value, family: &Value| -> Vec<CatalogEntry> {
        let family_label = jstr(family, "family_label");
        let model_uid = jstr(variant, "model_uid");
        let label = {
            let label = jstr(variant, "label");
            if label.is_empty() {
                jstr(variant, "name")
            } else {
                label
            }
        };
        if model_uid.is_empty() || label.is_empty() {
            return Vec::new();
        }
        let mut description = jstr(variant, "description");
        let cost = jstr(variant, "cost_summary");
        if !cost.is_empty() {
            if !description.is_empty() {
                description.push_str(" · ");
            }
            description.push_str(&cost);
        }
        vec![CatalogEntry {
            value: model_uid,
            name: if label == family_label {
                label
            } else {
                format!("{family_label} · {label}")
            },
            description,
        }]
    };
    if let Some(families) = v.get("families").and_then(Value::as_array) {
        return families
            .iter()
            .flat_map(|family| {
                family
                    .get("variants")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .flat_map(|v| variants(v, family))
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
            .collect();
    }
    v.as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|e| {
                    let value = jstr(e, "value");
                    if value.is_empty() {
                        return None;
                    }
                    Some(CatalogEntry {
                        value,
                        name: jstr(e, "name"),
                        description: jstr(e, "description"),
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// fetch_model_catalog runs the agent's models_command (first element is
/// the program, the rest its args) and parses stdout. Errors surface —
/// the caller decides whether to show a bare options list.
pub async fn fetch_model_catalog(cfg: &AcpAgentConfig) -> Result<Vec<CatalogEntry>, String> {
    let (program, args) = cfg
        .models_command
        .split_first()
        .ok_or("no models command")?;
    let mut cmd = tokio::process::Command::new(program);
    cmd.args(args).stdin(std::process::Stdio::null());
    let out = tokio::time::timeout(Duration::from_secs(15), cmd.output())
        .await
        .map_err(|_| "models command timed out".to_string())?
        .map_err(|e| format!("models command {program}: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stderr = stderr.lines().next().unwrap_or("failed");
        return Err(format!("models command {program} failed: {stderr}"));
    }
    Ok(parse_model_catalog(&String::from_utf8_lossy(&out.stdout)))
}

/// A session/new (or session/load) probe result: everything agents use
/// to advertise their selectable surface.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionConfigProbe {
    pub session_id: String,
    /// Raw configOptions, notification updates merged in (they race the
    /// response and carry newer values).
    pub config_options: Vec<ConfigOption>,
    pub models: Option<SessionModels>,
    pub modes: AgentModes,
}

impl SessionConfigProbe {
    /// Rows for the picker: raw config options, plus synthesized model
    /// /mode options for agents that only speak the spec's models/modes
    /// objects (their selections ride session/set_model / set_mode).
    pub fn picker_options(&self) -> Vec<ConfigOption> {
        let mut options = self.config_options.clone();
        let models = match &self.models {
            Some(m) => m,
            None => return options,
        };
        if config_option_for_category(&options, "model").is_some() {
            return options;
        }
        options.push(ConfigOption {
            id: "model".into(),
            name: "Model".into(),
            description: "Models the agent offers (session/set_model)".into(),
            category: "model".into(),
            typ: "select".into(),
            current_value: json!(models.current_model_id),
            options: models
                .available
                .iter()
                .map(|m| ConfigOptionValue {
                    value: m.model_id.clone(),
                    name: if m.name.is_empty() {
                        m.model_id.clone()
                    } else {
                        m.name.clone()
                    },
                    description: m.description.clone(),
                })
                .collect(),
        });
        options
    }
}

/// options_with_model_catalog folds a models_command catalog into the
/// picker's model rows: advertised values keep their positions, catalog
/// entries the agent didn't list are appended.
pub fn options_with_model_catalog(
    options: &[ConfigOption],
    catalog: &[CatalogEntry],
) -> Vec<ConfigOption> {
    if catalog.is_empty() {
        return options.to_vec();
    }
    let mut options = options.to_vec();
    let Some(model) = options.iter_mut().find(|o| o.category == "model") else {
        return options;
    };
    for entry in catalog {
        if model.options.iter().any(|v| v.value == entry.value) {
            continue;
        }
        model.options.push(ConfigOptionValue {
            value: entry.value.clone(),
            name: if entry.name.is_empty() {
                entry.value.clone()
            } else {
                entry.name.clone()
            },
            description: entry.description.clone(),
        });
    }
    options
}

/// call_collecting_options runs a session-level request while draining
/// notifications: agents that announce config options via
/// config_option_update (Devin) send them as session/update frames that
/// race the response, and they carry the newest values. Returns the
/// response and any options seen along the way.
pub async fn call_collecting_options(
    conn: &Arc<AcpConnection>,
    method: &str,
    params: Value,
) -> Result<(Value, Vec<ConfigOption>), AcpError> {
    let mut notifs = conn.subscribe();
    let call_conn = conn.clone();
    let method_owned = method.to_string();
    let task = tokio::spawn(async move { call_conn.call(&method_owned, params).await });
    tokio::pin!(task);
    let mut notif_options: Vec<ConfigOption> = Vec::new();
    let result = loop {
        tokio::select! {
            n = notifs.recv() => match n {
                Ok((_, params)) => {
                    let update = params.get("update");
                    if update
                        .and_then(|u| u.get("sessionUpdate"))
                        .and_then(Value::as_str)
                        == Some("config_option_update")
                    {
                        if let Some(incoming) =
                            update.and_then(|u| u.get("configOptions")).map(parse_config_options)
                        {
                            notif_options = merge_config_options(notif_options, incoming);
                        }
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => {
                    return Err(AcpError::new(-32603, "agent notification channel closed"));
                }
            },
            res = &mut task => {
                break res
                    .map_err(|e| AcpError::new(-32603, format!("{method} task died: {e}")))?
                    .map_err(|e| {
                        // The call task may still be draining notifications;
                        // the response (or error) is terminal either way.
                        e
                    })?;
            }
        }
    };
    Ok((result, notif_options))
}

/// probe_session_config runs session/new with notification capture (see
/// call_collecting_options). `params` is the full session/new payload
/// (`cwd`, `mcpServers`, optional `_meta`). When the agent advertises a
/// model option with no values (Devin ships it empty until a set
/// round-trip), the current value is re-set once to force a populated
/// refresh.
pub async fn probe_session_config(
    conn: &Arc<AcpConnection>,
    params: Value,
) -> Result<SessionConfigProbe, String> {
    let (result, notif_options) = call_collecting_options(conn, "session/new", params)
        .await
        .map_err(|e| e.to_string())?;
    let session_id = jstr(&result, "sessionId");
    if session_id.is_empty() {
        return Err("session/new returned no sessionId".into());
    }
    let config_options = parse_config_options(result.get("configOptions").unwrap_or(&Value::Null));
    let probe = session_config_from_result(session_id, result, config_options, notif_options);
    prime_model_option(conn, probe).await
}

/// probe_from_result assembles the probe from a session/new / load
/// response plus any notification-captured option updates. Takes the
/// session id explicitly — session/load doesn't echo it.
pub fn session_config_from_result(
    session_id: String,
    result: Value,
    config_options: Vec<ConfigOption>,
    notif_options: Vec<ConfigOption>,
) -> SessionConfigProbe {
    SessionConfigProbe {
        session_id,
        config_options: merge_config_options(config_options, notif_options),
        models: parse_session_models(result.get("models").unwrap_or(&Value::Null)),
        modes: caps_to_modes(result.get("modes").cloned().unwrap_or(Value::Null)),
    }
}

fn caps_to_modes(v: Value) -> AgentModes {
    AgentModes {
        current_mode_id: jstr(&v, "currentModeId"),
        available: v
            .get("availableModes")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|m| (jstr(m, "id"), jstr(m, "name")))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// prime_model_option works around agents (Devin) that advertise a
/// model config option with an empty value list: re-selecting the
/// current value makes the agent return the option populated, and
/// refreshes the other options with it.
async fn prime_model_option(
    conn: &Arc<AcpConnection>,
    mut probe: SessionConfigProbe,
) -> Result<SessionConfigProbe, String> {
    let current = config_option_for_category(&probe.config_options, "model")
        .filter(|o| o.options.is_empty())
        .and_then(|o| {
            o.current_value
                .as_str()
                .filter(|v| !v.is_empty())
                .map(str::to_owned)
        });
    let Some(current) = current else {
        return Ok(probe);
    };
    if let Ok(result) = conn
        .call(
            "session/set_config_option",
            json!({
                "sessionId": probe.session_id,
                "configId": "model",
                "value": current,
            }),
        )
        .await
    {
        if let Some(options) = result.get("configOptions") {
            probe.config_options =
                merge_config_options(probe.config_options, parse_config_options(options));
        }
    }
    Ok(probe)
}

pub fn parse_modes(caps: &AgentCapabilities) -> AgentModes {
    let Some(modes) = &caps.modes else {
        return AgentModes::default();
    };
    AgentModes {
        current_mode_id: jstr(modes, "currentModeId"),
        available: modes
            .get("available")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|m| (jstr(m, "id"), jstr(m, "name")))
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// One permission option the agent offers on session/request_permission.
#[derive(Debug, Clone, PartialEq)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    /// allow_once | allow_always | reject_once | reject_always
    pub kind: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct PermissionRequest {
    pub session_id: String,
    pub tool_call_id: String,
    pub title: String,
    pub kind: String,
    pub raw_input: Option<Value>,
    pub options: Vec<PermissionOption>,
}

/// extractPermissionRequest decodes a session/request_permission params
/// object; malformed params yield an empty request (the caller rejects).
pub fn parse_permission_request(params: &Value) -> PermissionRequest {
    let tc = params.get("toolCall").cloned().unwrap_or(Value::Null);
    PermissionRequest {
        session_id: jstr(params, "sessionId"),
        tool_call_id: jstr(&tc, "toolCallId"),
        title: jstr(&tc, "title"),
        kind: jstr(&tc, "kind"),
        raw_input: tc.get("rawInput").filter(|v| !v.is_null()).cloned(),
        options: params
            .get("options")
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .map(|o| PermissionOption {
                        id: jstr(o, "optionId"),
                        name: jstr(o, "name"),
                        kind: jstr(o, "kind"),
                    })
                    .collect()
            })
            .unwrap_or_default(),
    }
}

/// ACP error codes (JSON-RPC standard set; ACP adds no new codes).
pub const ERR_METHOD_NOT_FOUND: i64 = -32601;
pub const ERR_UNSUPPORTED_PROTOCOL: i64 = -32602;

#[derive(Debug, Clone, PartialEq)]
pub struct AcpError {
    pub code: i64,
    pub message: String,
}

impl AcpError {
    pub fn new(code: i64, message: impl Into<String>) -> Self {
        AcpError {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for AcpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "acp error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for AcpError {}

// ---------------------------------------------------------------------------
// Connection.
// ---------------------------------------------------------------------------

/// The client-side callbacks an agent can invoke. All paths are
/// absolute per the spec; the server implements fs itself and maps
/// permissions onto its approval prompt flow.
#[async_trait::async_trait]
pub trait AcpClientHandler: Send + Sync + 'static {
    async fn on_request(&self, method: &str, params: &Value) -> Result<Value, AcpError>;
}

/// Messages routed off the reader task.
pub struct AcpConnection {
    name: String,
    child: Mutex<tokio::process::Child>,
    write_tx: mpsc::Sender<String>,
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<Result<Value, AcpError>>>>,
    /// Fan-out for notifications (session/update and friends); readers
    /// filter by the sessionId in the params.
    notif_tx: tokio::sync::broadcast::Sender<(String, Value)>,
    /// Set to true when the agent's stdout closes (process gone); lets
    /// in-flight calls fail immediately instead of waiting out their
    /// timeout.
    closed_tx: tokio::sync::watch::Sender<bool>,
}

impl AcpConnection {
    /// spawn launches the agent process and its reader/writer tasks.
    /// The handler (when given) serves agent → client requests; without
    /// one, requests are answered method-not-found.
    pub fn spawn(
        name: &str,
        cfg: &AcpAgentConfig,
        cwd: &Path,
        handler: Option<Arc<dyn AcpClientHandler>>,
    ) -> Result<Arc<AcpConnection>, String> {
        if cfg.command.is_empty() {
            return Err(format!("acp agent {name}: missing command"));
        }
        if !cwd.is_absolute() {
            return Err(format!("acp agent {name}: cwd must be absolute"));
        }
        let mut cmd = tokio::process::Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .current_dir(cwd)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        for (k, v) in crate::mcp::expand_env_map(&cfg.env) {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("acp agent {name}: failed to launch {}: {e}", cfg.command))?;
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = child.stdout.take().expect("piped stdout");

        let (write_tx, mut write_rx) = mpsc::channel::<String>(64);
        tokio::spawn(async move {
            use tokio::io::AsyncWriteExt;
            let mut stdin = stdin;
            while let Some(line) = write_rx.recv().await {
                if stdin.write_all(line.as_bytes()).await.is_err()
                    || stdin.write_all(b"\n").await.is_err()
                {
                    break;
                }
                let _ = stdin.flush().await;
            }
        });

        let (notif_tx, _) = tokio::sync::broadcast::channel::<(String, Value)>(1024);
        let (closed_tx, _) = tokio::sync::watch::channel(false);
        let conn = Arc::new(AcpConnection {
            name: name.to_string(),
            child: Mutex::new(child),
            write_tx,
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            notif_tx,
            closed_tx,
        });

        // Reader: frame-delimited JSON-RPC demux.
        let reader_conn = conn.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut reader = BufReader::new(stdout);
            let mut buf = Vec::new();
            loop {
                buf.clear();
                match reader.read_until(b'\n', &mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let line = String::from_utf8_lossy(&buf)
                    .trim_end_matches(['\n', '\r'])
                    .to_string();
                let v = match serde_json::from_str::<Value>(&line) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                let has_method = v.get("method").and_then(Value::as_str).is_some();
                let id = v.get("id").and_then(Value::as_u64);
                let result = v.get("result");
                let error = v.get("error");
                if !has_method && (result.is_some() || error.is_some()) {
                    if let Some(id) = id {
                        let res = if let Some(err) = error {
                            Err(AcpError {
                                code: err.get("code").and_then(Value::as_i64).unwrap_or(-32603),
                                message: jstr(err, "message"),
                            })
                        } else {
                            Ok(result.cloned().unwrap_or(Value::Null))
                        };
                        let tx = reader_conn.pending.lock().unwrap().remove(&id);
                        if let Some(tx) = tx {
                            let _ = tx.send(res);
                        }
                    }
                } else if has_method {
                    let method = jstr(&v, "method");
                    let params = v.get("params").cloned().unwrap_or(Value::Null);
                    if let Some(id) = id {
                        // Agent → client request (fs, permission, …):
                        // served inline by the handler, or refused.
                        let res = match &handler {
                            Some(h) => h.on_request(&method, &params).await,
                            None => Err(AcpError::new(
                                ERR_METHOD_NOT_FOUND,
                                format!("method {method} is not supported"),
                            )),
                        };
                        let mut resp = serde_json::Map::new();
                        resp.insert("jsonrpc".into(), json!("2.0"));
                        resp.insert("id".into(), json!(id));
                        match res {
                            Ok(result) => {
                                resp.insert("result".into(), result);
                            }
                            Err(err) => {
                                resp.insert(
                                    "error".into(),
                                    json!({"code": err.code, "message": err.message}),
                                );
                            }
                        }
                        let _ = reader_conn
                            .write_tx
                            .send(Value::Object(resp).to_string())
                            .await;
                    } else {
                        let _ = reader_conn.notif_tx.send((method, params)).ok();
                    }
                }
            }
            // Process died: signal it, then fail every outstanding call
            // immediately. send_replace (not send) so the flag is stored
            // even when no receiver is subscribed yet — a call made
            // after exit must still see the closed state.
            let _ = reader_conn.closed_tx.send_replace(true);
            let pending = {
                let mut map = reader_conn.pending.lock().unwrap();
                std::mem::take(&mut *map)
            };
            for (_, tx) in pending {
                let _ = tx.send(Err(AcpError::new(
                    -32603,
                    "agent process exited before responding",
                )));
            }
        });

        Ok(conn)
    }

    /// subscribe returns a receiver for the connection's notifications
    /// (session/update and friends). The turn driver filters by the
    /// sessionId carried in the params.
    pub fn subscribe(&self) -> tokio::sync::broadcast::Receiver<(String, Value)> {
        self.notif_tx.subscribe()
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn is_alive(&self) -> bool {
        self.child
            .lock()
            .unwrap()
            .try_wait()
            .ok()
            .flatten()
            .is_none()
    }

    fn send_line(&self, line: &str) -> Result<(), AcpError> {
        self.write_tx
            .try_send(line.to_string())
            .map_err(|_| AcpError::new(-32603, "agent process is gone"))
    }

    /// call sends a request and awaits its response.
    pub async fn call(&self, method: &str, params: Value) -> Result<Value, AcpError> {
        self.call_timeout(method, params, Some(CALL_TIMEOUT)).await
    }

    pub async fn call_timeout(
        &self,
        method: &str,
        params: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, AcpError> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params});
        if let Err(err) = self.send_line(&line.to_string()) {
            self.pending.lock().unwrap().remove(&id);
            return Err(err);
        }
        let mut closed = self.closed_tx.subscribe();
        let fut = async {
            // An already-closed process fails immediately; otherwise the
            // watch fires when the agent's stdout closes.
            if *closed.borrow_and_update() {
                Err(AcpError::new(-32603, "agent process exited"))
            } else {
                tokio::select! {
                    res = rx => res.unwrap_or_else(|_| {
                        Err(AcpError::new(-32603, "call dropped"))
                    }),
                    _ = closed.changed() => {
                        Err(AcpError::new(-32603, "agent process exited"))
                    }
                }
            }
        };
        match timeout {
            Some(t) => match tokio::time::timeout(t, fut).await {
                Ok(Ok(res)) => Ok(res),
                Ok(Err(err)) => Err(err),
                Err(_) => {
                    self.pending.lock().unwrap().remove(&id);
                    Err(AcpError::new(-32603, format!("{method} timed out")))
                }
            },
            None => match fut.await {
                Ok(res) => Ok(res),
                Err(err) => Err(err),
            },
        }
    }

    /// notify sends a notification (no response expected).
    pub async fn notify(&self, method: &str, params: Value) -> Result<(), AcpError> {
        let line = json!({"jsonrpc": "2.0", "method": method, "params": params});
        self.send_line(&line.to_string())
    }

    /// initialize negotiates the protocol version and returns the
    /// agent's capabilities.
    pub async fn initialize(&self) -> Result<InitializeResult, AcpError> {
        let result = self
            .call_timeout(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "clientCapabilities": {
                        "fs": {"readTextFile": true, "writeTextFile": true},
                        "terminal": false,
                    },
                }),
                Some(INITIALIZE_TIMEOUT),
            )
            .await?;
        let parsed: InitializeResult = serde_json::from_value(result)
            .map_err(|e| AcpError::new(-32602, format!("bad initialize result: {e}")))?;
        if parsed.protocol_version > PROTOCOL_VERSION {
            return Err(AcpError::new(
                ERR_UNSUPPORTED_PROTOCOL,
                format!(
                    "agent speaks protocol v{}, atom supports v{PROTOCOL_VERSION}",
                    parsed.protocol_version
                ),
            ));
        }
        Ok(parsed)
    }
}

impl Drop for AcpConnection {
    fn drop(&mut self) {
        if let Ok(mut child) = self.child.lock() {
            let _ = child.start_kill();
        }
    }
}

// ---------------------------------------------------------------------------
// Connection hub: one process per (agent, cwd), shared across sessions.
// ---------------------------------------------------------------------------

static HUB: Lazy<tokio::sync::Mutex<HashMap<String, AgentProcess>>> =
    Lazy::new(|| tokio::sync::Mutex::new(HashMap::new()));

#[derive(Clone)]
pub struct AgentProcess {
    pub conn: Arc<AcpConnection>,
    pub capabilities: AgentCapabilities,
}

/// get_connection returns a live connection for the agent, launching it
/// (with the given client handler) if needed. Dead connections are
/// replaced. Holding the async hub lock across spawn+initialize keeps
/// concurrent turns from double-spawning the same agent.
pub async fn get_connection(
    name: &str,
    cfg: &AcpAgentConfig,
    cwd: &Path,
    handler: Option<Arc<dyn AcpClientHandler>>,
) -> Result<AgentProcess, String> {
    let key = format!("{name}\x00{}", cwd.to_string_lossy());
    let mut hub = HUB.lock().await;
    if let Some(agent) = hub.get(&key) {
        if agent.conn.is_alive() {
            return Ok(AgentProcess {
                conn: agent.conn.clone(),
                capabilities: agent.capabilities.clone(),
            });
        }
    }
    let conn = AcpConnection::spawn(name, cfg, cwd, handler)?;
    let initialize = conn
        .initialize()
        .await
        .map_err(|e| format!("acp agent {name}: initialize failed: {e}"))?;
    let agent = AgentProcess {
        conn: conn.clone(),
        capabilities: initialize.agent_capabilities,
    };
    hub.insert(key, agent.clone());
    Ok(agent)
}

/// close_all_acp drops the hub on server shutdown.
pub fn close_all_acp() {
    if let Ok(mut hub) = HUB.try_lock() {
        hub.clear();
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn deprecated_claude_package_is_migrated_on_load() {
        let dir = std::env::temp_dir().join(format!("acp-migrate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("acp.json"),
            r#"{"acpAgents":{"claude-code":{"command":"npx","args":["-y","@zed-industries/claude-code-acp"]}}}"#,
        )
        .unwrap();
        let mut out = BTreeMap::new();
        merge_acp_file(&mut out, &dir.join("acp.json"));
        assert_eq!(out["claude-code"].args, vec!["-y", CLAUDE_ACP_PACKAGE]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    use super::*;
    use std::sync::atomic::AtomicUsize;

    /// A shell-based ACP agent: echoes requests with results shaped
    /// {"ok": true}, sends one fs/write_text_file request aimed at the
    /// client, and on session/prompt emits one agent_message_chunk
    /// notification before answering with the real request id.
    fn fake_agent_script() -> String {
        r#"while IFS= read -r line; do
  case "$line" in
    *'"fs/write_text_file"'*)
      echo '{"jsonrpc":"2.0","id":77,"method":"fs/write_text_file","params":{"path":"/tmp/acp-test","content":"agent wrote this"}}'
      ;;
    *'"session/prompt"'*)
      echo '{"jsonrpc":"2.0","id":77,"method":"fs/write_text_file","params":{"path":"/tmp/acp-test","content":"agent wrote this"}}'
      echo '{"jsonrpc":"2.0","method":"session/update","params":{"sessionId":"s1","update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"agent reply"}}}}'
      id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
      echo '{"jsonrpc":"2.0","id":'"$id"',"result":{"stopReason":"end_turn"}}'
      ;;
    *)
      resp=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/{"jsonrpc":"2.0","id":\1,"result":{"ok":true}}/p')
      [ -n "$resp" ] && echo "$resp"
      ;;
  esac
done
"#
        .to_string()
    }

    async fn spawn_fake(dir: &Path) -> Arc<AcpConnection> {
        let cfg = AcpAgentConfig {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), fake_agent_script()],
            ..Default::default()
        };
        let conn = AcpConnection::spawn("fake", &cfg, dir, None).unwrap();
        conn.initialize().await.unwrap();
        conn
    }

    #[tokio::test]
    async fn initialize_negotiates_and_calls_work() {
        let dir = tempfile::tempdir().unwrap();
        let conn = spawn_fake(dir.path()).await;
        assert_eq!(conn.name(), "fake");
        assert!(conn.is_alive());
        let res = conn
            .call("session/new", json!({"cwd": "/tmp"}))
            .await
            .unwrap();
        assert_eq!(res["ok"], json!(true));
    }

    #[tokio::test]
    async fn notifications_are_demuxed_from_responses() {
        let dir = tempfile::tempdir().unwrap();
        let conn = spawn_fake(dir.path()).await;
        let mut notifs = conn.subscribe();
        let res = conn
            .call("session/prompt", json!({"sessionId": "s1"}))
            .await
            .unwrap();
        assert_eq!(res["stopReason"], json!("end_turn"));
        // The agent_message_chunk notification arrived on the broadcast
        // (not swallowed as a response).
        let (_, params) = tokio::time::timeout(Duration::from_secs(5), notifs.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            params["update"]["sessionUpdate"],
            json!("agent_message_chunk")
        );
    }

    #[tokio::test]
    async fn agent_client_requests_are_served_by_handler() {
        let dir = tempfile::tempdir().unwrap();
        let writes = Arc::new(AtomicUsize::new(0));
        struct H {
            writes: Arc<AtomicUsize>,
        }
        #[async_trait::async_trait]
        impl AcpClientHandler for H {
            async fn on_request(&self, method: &str, _params: &Value) -> Result<Value, AcpError> {
                match method {
                    "fs/write_text_file" => {
                        self.writes.fetch_add(1, Ordering::SeqCst);
                        Ok(json!({}))
                    }
                    _ => Err(AcpError::new(ERR_METHOD_NOT_FOUND, "no")),
                }
            }
        }
        let cfg = AcpAgentConfig {
            command: "/bin/sh".into(),
            args: vec!["-c".into(), fake_agent_script()],
            ..Default::default()
        };
        let conn = AcpConnection::spawn(
            "fake",
            &cfg,
            dir.path(),
            Some(Arc::new(H {
                writes: writes.clone(),
            })),
        )
        .unwrap();
        conn.initialize().await.unwrap();
        conn.call("session/prompt", json!({"sessionId": "s1"}))
            .await
            .unwrap();
        // The agent's fs/write_text_file request reached the handler.
        for _ in 0..50 {
            if writes.load(Ordering::SeqCst) == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(writes.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn dead_process_fails_calls_with_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        eprintln!("dbg: spawn");
        let conn = spawn_fake(dir.path()).await;
        eprintln!("dbg: kill");
        conn.child.lock().unwrap().start_kill().unwrap();
        for _ in 0..50 {
            if !conn.is_alive() {
                eprintln!("dbg: dead");
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        eprintln!("dbg: calling");
        let err = conn.call("session/new", json!({})).await.unwrap_err();
        eprintln!("dbg: got error {err}");
        assert!(!err.message.is_empty());
    }

    #[test]
    fn acp_config_discovery_overrides_and_disables() {
        let cfg_dir = tempfile::tempdir().unwrap();
        let proj = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(proj.path().join(".atom")).unwrap();
        std::fs::write(
            cfg_dir.path().join("acp.json"),
            r#"{"acpAgents": {
                "claude-code": {"command": "npx", "args": ["-y", "@zed-industries/claude-code-acp"]},
                "gone": {"command": "x", "disabled": true}
            }}"#,
        )
        .unwrap();
        std::fs::write(
            proj.path().join(".atom/acp.json"),
            r#"{"acpAgents": {
                "claude-code": {"command": "claude-code-acp"}
            }}"#,
        )
        .unwrap();
        let out = load_acp_configs_in(
            proj.path().to_str().unwrap(),
            Some(cfg_dir.path()),
            Some(proj.path()),
        );
        assert_eq!(
            out.get("claude-code").unwrap().command,
            "claude-code-acp",
            "project overrides user config"
        );
        assert!(!out.contains_key("gone"), "disabled entries are removed");
    }

    #[test]
    fn updates_parse_across_the_spec_shapes() {
        let u = parse_update(&json!({
            "sessionUpdate": "tool_call",
            "toolCallId": "t1",
            "title": "Edit src/main.rs",
            "kind": "edit",
            "status": "completed",
            "rawInput": {"path": "src/main.rs"},
            "content": [{"type": "diff", "path": "src/main.rs", "oldText": "a", "newText": "b"}]
        }))
        .unwrap();
        match u {
            Update::ToolCall {
                id,
                title,
                kind,
                status,
                contents,
                raw_input,
            } => {
                assert_eq!(id, "t1");
                assert_eq!(title, "Edit src/main.rs");
                assert_eq!(kind, "edit");
                assert_eq!(status, "completed");
                assert_eq!(contents.len(), 1);
                assert_eq!(contents[0].typ, "diff");
                assert_eq!(contents[0].new_text.as_deref(), Some("b"));
                assert_eq!(raw_input.unwrap()["path"], json!("src/main.rs"));
            }
            other => panic!("wrong variant: {other:?}"),
        }
        // Unknown discriminators are skipped, not errors.
        assert!(parse_update(&json!({"sessionUpdate": "future_thing"})).is_none());
        // Message chunks can carry a single block object or an array.
        let u = parse_update(&json!({
            "sessionUpdate": "agent_message_chunk",
            "content": {"type": "text", "text": "hi"}
        }))
        .unwrap();
        match u {
            Update::AgentMessageChunk { content } => {
                assert_eq!(content.len(), 1);
                assert_eq!(content[0].text, "hi");
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn permission_requests_parse_loosely() {
        let req = parse_permission_request(&json!({
            "sessionId": "s1",
            "options": [
                {"optionId": "o1", "name": "Allow once", "kind": "allow_once"},
                {"optionId": "o2", "name": "Reject", "kind": "reject_once"}
            ],
            "toolCall": {"toolCallId": "t1", "title": "rm -rf", "kind": "execute",
                          "rawInput": {"command": "rm -rf"}}
        }));
        assert_eq!(req.session_id, "s1");
        assert_eq!(req.tool_call_id, "t1");
        assert_eq!(req.kind, "execute");
        assert_eq!(req.options.len(), 2);
        assert_eq!(req.options[0].kind, "allow_once");
        assert_eq!(req.raw_input.unwrap()["command"], json!("rm -rf"));
    }

    #[test]
    fn bundled_agents_cover_claude_code_and_gemini() {
        let agents = bundled_acp_agents();
        let (claude_id, _, claude_cfg) = agents
            .iter()
            .find(|(id, _, _)| *id == "claude-code")
            .expect("claude-code bundled");
        assert_eq!(*claude_id, "claude-code");
        assert_eq!(claude_cfg.command, "npx");
        assert!(claude_cfg.args.iter().any(|a| a == CLAUDE_ACP_PACKAGE));
        assert!(agents.iter().any(|(id, _, _)| *id == "gemini"));
        assert!(agents.iter().any(|(id, _, _)| *id == "codex"));
        assert!(agents.iter().any(|(id, _, _)| *id == "devin"));
    }

    #[test]
    fn add_and_remove_agent_round_trip_the_user_acp_json() {
        let _guard = crate::testutil::env_lock();
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: serialized behind env_lock; test-only process env.
        unsafe { std::env::set_var("XDG_CONFIG_HOME", dir.path()) };
        let leaf = atom_core::build::dir_leaf();
        let path = dir.path().join(&leaf).join("acp.json");

        let cfg = AcpAgentConfig {
            command: "npx".into(),
            args: vec!["-y".into(), "@zed-industries/claude-code-acp".into()],
            ..Default::default()
        };
        add_agent("claude-code", &cfg).unwrap();
        let loaded = load_acp_configs_in("/tmp", Some(&dir.path().join(&leaf)), None);
        assert_eq!(loaded.get("claude-code").unwrap().command, "npx");

        // Re-adding preserves siblings and unknown top-level keys.
        std::fs::write(
            &path,
            r#"{"acpAgents": {"other": {"command": "x"}}, "custom": 1}"#,
        )
        .unwrap();
        add_agent("claude-code", &cfg).unwrap();
        let raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert_eq!(raw["custom"], json!(1), "unknown keys preserved");
        assert_eq!(raw["acpAgents"]["other"]["command"], json!("x"));

        assert!(remove_agent("claude-code").unwrap());
        assert!(!remove_agent("claude-code").unwrap());
        let raw: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        assert!(raw["acpAgents"].get("claude-code").is_none());
        assert_eq!(raw["acpAgents"]["other"]["command"], json!("x"));

        // SAFETY: restore before releasing env_lock.
        unsafe { std::env::remove_var("XDG_CONFIG_HOME") };
    }
}
