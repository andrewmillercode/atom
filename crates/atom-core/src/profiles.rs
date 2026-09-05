//! Agent profiles: named model/thinking/instruction presets stored as
//! markdown files in `<config_dir>/agents/*.md`. Frontmatter defines
//! the profile (`name`, `model`, `thinking`); the rest of the file is
//! the prompt, used as extra instructions for subagents spawned with
//! the profile. The implicit "default" profile (empty everything,
//! hidden in the UI) needs no file.

use crate::config::config_dir;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgentProfile {
    pub name: String,
    /// Model id, optionally `provider/model`; empty inherits the
    /// session's model.
    pub model: String,
    /// Reasoning effort; empty inherits the session's level.
    pub thinking: String,
    /// Prompt below the frontmatter; extra subagent instructions.
    pub body: String,
}

fn agents_dir() -> std::path::PathBuf {
    config_dir().join("agents")
}

/// loadProfiles reads `<config_dir>/agents/*.md` in filename order,
/// seeding plan.md and build.md on first run. The implicit default
/// profile is prepended so a profile index of 0 means "no profile".
pub fn load_profiles() -> Vec<AgentProfile> {
    load_profiles_from(&agents_dir())
}

/// Cycle order of the seeded profiles, ahead of any user-added ones.
const SEED_ORDER: [&str; 2] = ["plan", "build"];

fn load_profiles_from(dir: &std::path::Path) -> Vec<AgentProfile> {
    seed_default_profiles(dir);
    // Slot 0 is the implicit default — the no-profile state. Its empty
    // name keeps it hidden in the UI.
    let mut profiles = vec![AgentProfile::default()];
    let Ok(entries) = std::fs::read_dir(dir) else {
        return profiles;
    };
    let mut files: Vec<_> = entries
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|e| e == "md"))
        .map(|e| e.path())
        .collect();
    files.sort_by_key(|path| {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        (
            SEED_ORDER
                .iter()
                .position(|s| *s == stem)
                .unwrap_or(SEED_ORDER.len()),
            stem,
        )
    });
    for path in files {
        if let Ok(content) = std::fs::read_to_string(&path) {
            let profile = parse_profile(&content);
            // A profile needs a name; model and thinking are optional.
            if !profile.name.is_empty() {
                profiles.push(profile);
            }
        }
    }
    profiles
}

/// modelRef splits a model id that may carry a provider prefix:
/// `hyper/glm-5.3-flash` becomes `("hyper", "glm-5.3-flash")`; a bare
/// id has no provider and relies on catalog lookup or inheritance.
pub fn model_ref(model: &str) -> (Option<&str>, &str) {
    match model.split_once('/') {
        Some((provider, model)) => (Some(provider), model),
        None => (None, model),
    }
}

/// findProfile resolves a subagent tool `agent` argument; the implicit
/// "default" profile inherits everything.
pub fn find_profile(name: &str) -> Option<AgentProfile> {
    if name == "default" {
        return Some(AgentProfile {
            name: name.to_string(),
            ..Default::default()
        });
    }
    load_profiles().into_iter().find(|p| p.name == name)
}

/// parseProfile reads a `---` fenced frontmatter block; the rest of the
/// file is the prompt body. `name` is required; `model` and `thinking`
/// are optional (empty inherits the session's). Returns a profile with
/// an empty name when the file defines none — such files are not
/// profiles.
fn parse_profile(content: &str) -> AgentProfile {
    let mut profile = AgentProfile::default();
    let Some(rest) = content.trim_start().strip_prefix("---") else {
        return profile;
    };
    let mut remainder = rest;
    loop {
        let Some(line_end) = remainder.find('\n') else {
            return profile; // unterminated frontmatter: keys keep, no body
        };
        let line = remainder[..line_end].trim();
        remainder = &remainder[line_end + 1..];
        if line == "---" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            match key.trim() {
                "name" => profile.name = value.trim().to_string(),
                "model" => profile.model = value.trim().to_string(),
                "thinking" => profile.thinking = value.trim().to_string(),
                _ => {}
            }
        }
    }
    profile.body = remainder.trim().to_string();
    profile
}

/// Seed the two visible built-in profiles on first run only; user edits
/// and deletions are never overwritten.
fn seed_default_profiles(dir: &std::path::Path) {
    if dir.exists() {
        return;
    }
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    for (file, template) in [("plan.md", PLAN_TEMPLATE), ("build.md", BUILD_TEMPLATE)] {
        let _ = std::fs::write(dir.join(file), template);
    }
}

const PLAN_TEMPLATE: &str = "---
name: plan
---

You are the planning agent. You research problems and produce
comprehensive, actionable implementation plans. You do not implement
them — other agents (e.g. build) execute your plan.

READ-ONLY: never edit, create, or delete files; never run commands
that modify the system — no builds, tests, commits, or config changes.
Use only read-only tools: search, read, inspect.

Method:
- Understand the request and the actual code before proposing
  anything. Trace the real code paths; verify assumptions by reading
  the files involved rather than guessing.
- Explore broadly first (entry points, conventions, existing tests),
  then narrow to exactly what a change would touch.
- Prefer the smallest design that fully solves the problem; weigh
  tradeoffs explicitly when the choice is genuinely close.

The plan you deliver:
- States the goal and approach in a few sentences.
- Lists concrete steps in execution order, each naming the files or
  symbols it touches and what changes there.
- Calls out risks, edge cases, and anything you could not verify.
- Ends with a verification section: how to build, test, and confirm
  the change end to end.

Do not write code beyond short illustrative snippets. Deliver the
plan and stop; implementation belongs to other agents.
";

const BUILD_TEMPLATE: &str = "---
name: build
---

Build agent: implement, build, and test.
";
