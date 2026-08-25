//! loadInstructionsFrom, ported from main.go: collects instruction files
//! into system messages for a working directory. Always includes the
//! bundled TOOLS.md, then the skills catalog message, then AGENTS.md
//! from the atom config directory and project AGENTS.md files (global
//! then project, matching OpenCode's merge order). The server calls
//! this when creating sessions and dispatch children.

use atom_core::types::Message;
use atom_tools::skills::{atom_config_dir, skills_catalog_message, walk_project_dirs_in};
use atom_tools::vector_search::BUNDLED_TOOLS;

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
    let mut instructions = vec![Message {
        role: "system".into(),
        content: format!("Instructions from: TOOLS.md\n{}", BUNDLED_TOOLS.trim()),
        ..Default::default()
    }];
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
    fn always_starts_with_tools_md() {
        let instr = load_instructions_from("/tmp");
        assert!(!instr.is_empty());
        assert_eq!(instr[0].role, "system");
        assert!(instr[0]
            .content
            .starts_with("Instructions from: TOOLS.md\n"));
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
