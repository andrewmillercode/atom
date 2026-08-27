//! loadInstructionsFrom, ported from main.go: assembles the bundled
//! atom instructions (system-prompt.md, then tools.md from the repo's
//! instructions/ folder), then the skills catalog message, then AGENTS.md
//! from the atom config directory and project AGENTS.md files (global
//! then project, matching OpenCode's merge order). AGENTS.md files add
//! context in place; they never replace the bundled system prompt. The
//! server calls this when creating sessions and dispatch children.

use atom_core::types::Message;
use atom_tools::skills::{atom_config_dir, skills_catalog_message, walk_project_dirs_in};
use atom_tools::vector_search::BUNDLED_TOOLS;

/// Bundled instructions/system-prompt.md embedded like Go's //go:embed.
pub const BUNDLED_SYSTEM_PROMPT: &str = include_str!("../../../instructions/system-prompt.md");

/// bundled_message renders one of the repo's instruction files as a
/// system message labeled with its path inside instructions/.
fn bundled_message(name: &str, body: &str) -> Message {
    Message {
        role: "system".into(),
        content: format!("Instructions from: instructions/{name}\n{}", body.trim()),
        ..Default::default()
    }
}

/// readInstructionFile reads an AGENTS.md file and renders it as a
/// single system-instruction block in OpenCode's format. Read errors are
/// skipped silently: instruction files are optional.
fn read_instruction_file(path: &std::path::Path) -> Option<String> {
    let b = std::fs::read_to_string(path).ok()?;
    Some(format!(
        "Instructions from: {}\n{}",
        path.display(),
        b.trim()
    ))
}

pub fn load_instructions_from(cwd: &str) -> Vec<Message> {
    let mut instructions = vec![
        bundled_message("system-prompt.md", BUNDLED_SYSTEM_PROMPT),
        bundled_message("tools.md", BUNDLED_TOOLS),
    ];
    let catalog = skills_catalog_message(cwd);
    if !catalog.is_empty() {
        instructions.push(Message {
            role: "system".into(),
            content: catalog,
            ..Default::default()
        });
    }

    // Global source: AGENTS.md in the atom config directory
    // ($XDG_CONFIG_HOME/atom, defaulting to ~/.config/atom).
    if let Some(config_dir) = atom_config_dir() {
        if let Some(content) = read_instruction_file(&config_dir.join("AGENTS.md")) {
            instructions.push(Message {
                role: "system".into(),
                content,
                ..Default::default()
            });
        }
    }

    // Project source: walk from cwd up to the home directory, collecting
    // every AGENTS.md found. Closest-to-cwd files are added first and the
    // root-most last, matching OpenCode's findUp ordering. If cwd is
    // outside home, only cwd itself is checked.
    let home = dirs_home();
    for dir in walk_project_dirs_in(cwd, home.as_deref()) {
        if let Some(content) = read_instruction_file(&dir.join("AGENTS.md")) {
            instructions.push(Message {
                role: "system".into(),
                content,
                ..Default::default()
            });
        }
    }
    instructions
}

fn dirs_home() -> Option<std::path::PathBuf> {
    dirs::home_dir()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn starts_with_bundled_prompt_then_tools() {
        let instr = load_instructions_from("/tmp");
        assert!(instr.len() >= 2);
        assert_eq!(instr[0].role, "system");
        assert!(instr[0]
            .content
            .starts_with("Instructions from: instructions/system-prompt.md\n"));
        assert!(instr[1]
            .content
            .starts_with("Instructions from: instructions/tools.md\n"));
    }

    #[test]
    fn picks_up_project_agents_md_closest_first() {
        let root = tempfile::tempdir().unwrap();
        let sub = root.path().join("a").join("b");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(root.path().join("AGENTS.md"), "root rules").unwrap();
        std::fs::write(sub.join("AGENTS.md"), "leaf rules").unwrap();

        // Outside home only cwd itself is checked (tempdir is outside).
        let instr = load_instructions_from(&sub.display().to_string());
        let found: Vec<&str> = instr
            .iter()
            .filter(|m| m.content.contains("rules"))
            .map(|m| match () {
                _ if m.content.contains("leaf") => "leaf",
                _ => "root",
            })
            .collect();
        assert_eq!(
            found,
            vec!["leaf"],
            "only the closest AGENTS.md when outside home: {:?}",
            instr
        );
    }
}
