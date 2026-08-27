//! Skills catalog + skill tool, ported from skills.go: SKILL.md
//! frontmatter parsing, layered discovery (user catalogs first, project
//! catalogs closer to cwd override), the system-prompt catalog message,
//! and the skill tool body loader.

use serde::Deserialize;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

/// skill is one Cursor-compatible SKILL.md: catalog fields plus the body
/// loaded on demand by the skill tool.
#[derive(Debug, Clone, Default)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub dir: String,
}

#[derive(Deserialize, Default)]
struct SkillFrontmatter {
    #[serde(default)]
    name: String,
    #[serde(default)]
    description: String,
}

/// atomConfigDir is $XDG_CONFIG_HOME/atom, defaulting to ~/.config/atom
/// (atom-dev for dev builds — see atom_core::build).
pub fn atom_config_dir() -> Option<PathBuf> {
    let config_dir = match std::env::var("XDG_CONFIG_HOME") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        _ => dirs::home_dir()?.join(".config"),
    };
    Some(config_dir.join(atom_core::build::dir_leaf()))
}

/// walkProjectDirs lists cwd then each parent up to home (closest first).
/// If cwd is outside home, only cwd is returned.
pub fn walk_project_dirs(cwd: &str) -> Vec<PathBuf> {
    let home = dirs::home_dir();
    walk_project_dirs_in(cwd, home.as_deref())
}

pub fn walk_project_dirs_in(cwd: &str, home: Option<&Path>) -> Vec<PathBuf> {
    let mut cwd = if cwd.is_empty() {
        PathBuf::from("/")
    } else {
        PathBuf::from(cwd)
    };
    let Some(home) = home else {
        return vec![cwd];
    };
    let inside_home = cwd.starts_with(home);
    let mut dirs = Vec::new();
    loop {
        dirs.push(cwd.clone());
        if cwd == home || !inside_home || cwd == Path::new("/") {
            break;
        }
        let parent = match cwd.parent() {
            Some(p) if p != cwd => p.to_path_buf(),
            _ => break,
        };
        cwd = parent;
    }
    dirs
}

pub fn parse_skill_markdown(
    content: &str,
    fallback_name: &str,
    dir: &str,
) -> Result<Skill, String> {
    let mut s = Skill {
        name: fallback_name.to_string(),
        dir: dir.to_string(),
        ..Default::default()
    };
    let text = content.strip_prefix('\u{FEFF}').unwrap_or(content);
    let rest = text.trim_start_matches(['\r', '\n']);
    if !rest.starts_with("---") {
        s.body = text.trim().to_string();
        return Ok(s);
    }
    let rest = &rest[3..];
    let rest = if let Some(r) = rest.strip_prefix("\r\n") {
        r
    } else if let Some(r) = rest.strip_prefix('\n') {
        r
    } else {
        rest
    };
    let Some(end) = rest.find("\n---") else {
        return Err("unclosed YAML frontmatter".to_string());
    };
    let fm = &rest[..end];
    let body = &rest[end + "\n---".len()..];
    let body = body.strip_prefix('\r').unwrap_or(body);
    let body = body.strip_prefix('\n').unwrap_or(body);

    let meta: SkillFrontmatter = serde_yaml::from_str(fm).map_err(|e| format!("yaml: {e}"))?;
    if !meta.name.trim().is_empty() {
        s.name = meta.name.trim().to_string();
    }
    s.description = meta.description.trim().to_string();
    s.body = body.trim().to_string();
    Ok(s)
}

fn load_skills_from_dir(root: &Path, into: &mut BTreeMap<String, Skill>) {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path().join("SKILL.md");
        let b = match std::fs::read(&path) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let dir = entry.path();
        let dir_str = dir.to_string_lossy().to_string();
        let fallback = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let parsed = match parse_skill_markdown(&String::from_utf8_lossy(&b), &fallback, &dir_str) {
            Ok(p) => p,
            Err(_) => continue,
        };
        if parsed.name.is_empty() {
            continue;
        }
        into.insert(parsed.name.clone(), parsed);
    }
}

/// discoverSkills returns skills keyed by name. User-level catalogs load
/// first; project catalogs closer to cwd override the same name.
pub fn discover_skills(cwd: &str) -> BTreeMap<String, Skill> {
    let config = atom_config_dir();
    let home = dirs::home_dir();
    discover_skills_in(cwd, config.as_deref(), home.as_deref())
}

pub fn discover_skills_in(
    cwd: &str,
    config_dir: Option<&Path>,
    home: Option<&Path>,
) -> BTreeMap<String, Skill> {
    let mut out = BTreeMap::new();
    if let Some(dir) = config_dir {
        load_skills_from_dir(&dir.join("skills"), &mut out);
    }
    if let Some(home) = home {
        load_skills_from_dir(&home.join(".agents").join("skills"), &mut out);
        load_skills_from_dir(&home.join(".cursor").join("skills"), &mut out);
    }
    let dirs = walk_project_dirs_in(cwd, home);
    for d in dirs.iter().rev() {
        load_skills_from_dir(&d.join(".atom").join("skills"), &mut out);
        load_skills_from_dir(&d.join(".cursor").join("skills"), &mut out);
        load_skills_from_dir(&d.join(".agents").join("skills"), &mut out);
    }
    out
}

/// skillsCatalogMessage renders the name+description list injected into
/// the system prompt. Empty when no skills exist.
pub fn skills_catalog_message(cwd: &str) -> String {
    let skills = discover_skills(cwd);
    if skills.is_empty() {
        return String::new();
    }
    let mut b = String::new();
    b.push_str("Instructions from: skills\n");
    b.push_str(
        "You can load extra instructions with the skill tool when a skill matches the task. Do not load a skill unless it is relevant.\n",
    );
    for (name, s) in &skills {
        b.push_str("\n- ");
        b.push_str(name);
        b.push_str(": ");
        b.push_str(&s.description);
    }
    b
}

pub fn execute_skill(arguments: &str, cwd: &str) -> String {
    let config = atom_config_dir();
    let home = dirs::home_dir();
    execute_skill_in(arguments, cwd, config.as_deref(), home.as_deref())
}

pub fn execute_skill_in(
    arguments: &str,
    cwd: &str,
    config_dir: Option<&Path>,
    home: Option<&Path>,
) -> String {
    #[derive(serde::Deserialize, Default)]
    struct Args {
        #[serde(default)]
        name: String,
    }
    let args: Args = match serde_json::from_str(arguments) {
        Ok(a) => a,
        Err(e) => return format!("error parsing arguments: {e}"),
    };
    let name = args.name.trim();
    let skills = discover_skills_in(cwd, config_dir, home);
    match skills.get(name) {
        Some(s) => format!("{}{}", s.body, skill_footer(s)),
        None => {
            if skills.is_empty() {
                return format!("error: unknown skill \"{name}\"");
            }
            let known: Vec<String> = skills.keys().cloned().collect();
            format!(
                "error: unknown skill \"{}\" (known: {})",
                name,
                known.join(", ")
            )
        }
    }
}

fn skill_footer(s: &Skill) -> String {
    let mut names = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&s.dir) {
        for e in entries.flatten() {
            let fname = e.file_name().to_string_lossy().to_string();
            if fname.eq_ignore_ascii_case("SKILL.md") {
                continue;
            }
            names.push(fname);
        }
    }
    names.sort();
    let mut b = format!("\n\nSkill directory: {}", s.dir);
    if !names.is_empty() {
        b.push_str("\nOther files: ");
        b.push_str(&names.join(", "));
    }
    b
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hermetic roots mirroring Go's hermeticHome: temp home + xdg + proj.
    fn hermetic() -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let home = tempfile::tempdir().unwrap();
        let xdg = home.path().join("xdg");
        let cwd = home.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let hb = home.path().to_path_buf();
        (home, xdg, cwd, hb)
    }

    fn write_skill_md(dir: &Path, name: &str, description: &str, body: &str) {
        std::fs::create_dir_all(dir).unwrap();
        let content = format!("---\nname: {name}\ndescription: {description}\n---\n{body}\n");
        std::fs::write(dir.join("SKILL.md"), content).unwrap();
    }

    #[test]
    fn parses_frontmatter_with_block_scalar() {
        let raw = "---\nname: example\ndescription: |-\n  When to use this skill\n  across two lines\n---\n# instructions\n\nDo the thing.\n";
        let s = parse_skill_markdown(raw, "fallback", "/tmp/example").unwrap();
        assert_eq!(s.name, "example");
        assert!(
            s.description.contains("When to use this skill")
                && s.description.contains("across two lines"),
            "{}",
            s.description
        );
        assert!(
            s.body.contains("# instructions") && s.body.contains("Do the thing."),
            "{}",
            s.body
        );
    }

    #[test]
    fn missing_name_falls_back_to_dir() {
        let s = parse_skill_markdown(
            "---\ndescription: d\n---\nbody\n",
            "parent-dir",
            "/tmp/parent-dir",
        )
        .unwrap();
        assert_eq!(s.name, "parent-dir");
        assert_eq!(s.body, "body");
    }

    #[test]
    fn unclosed_frontmatter_is_an_error() {
        assert!(parse_skill_markdown("---\nname: x\nbody", "f", "d").is_err());
    }

    #[test]
    fn no_frontmatter_means_whole_file_is_body() {
        let s = parse_skill_markdown("just text\n", "fb", "d").unwrap();
        assert_eq!(s.name, "fb");
        assert_eq!(s.body, "just text");
    }

    #[test]
    fn discovers_from_xdg_and_project_overrides_user() {
        let (_h, xdg, cwd, hb) = hermetic();
        write_skill_md(
            &xdg.join(atom_core::build::dir_leaf()).join("skills").join("hello"),
            "hello",
            "Say hi",
            "HELLO_BODY",
        );

        let skills = discover_skills_in(
            &cwd.display().to_string(),
            Some(&xdg.join(atom_core::build::dir_leaf())),
            Some(&hb),
        );
        let s = skills.get("hello").expect("missing hello");
        assert_eq!(s.description, "Say hi");
        assert_eq!(s.body, "HELLO_BODY");

        // User catalog defines demo first; project .cursor/skills wins.
        write_skill_md(
            &xdg.join(atom_core::build::dir_leaf()).join("skills").join("demo"),
            "demo",
            "user desc",
            "USER_BODY",
        );
        write_skill_md(
            &cwd.join(".cursor").join("skills").join("demo"),
            "demo",
            "project desc",
            "PROJECT_BODY",
        );

        let skills = discover_skills_in(
            &cwd.display().to_string(),
            Some(&xdg.join(atom_core::build::dir_leaf())),
            Some(&hb),
        );
        let s = skills.get("demo").expect("missing demo");
        assert_eq!(s.description, "project desc");
        assert_eq!(s.body, "PROJECT_BODY");
    }

    #[test]
    fn execute_skill_loads_body_with_footer() {
        let (_h, xdg, cwd, hb) = hermetic();
        let dir = xdg.join(atom_core::build::dir_leaf()).join("skills").join("pack");
        write_skill_md(&dir, "pack", "Pack files", "PACK_INSTRUCTIONS");
        std::fs::write(dir.join("helper.sh"), "echo hi").unwrap();

        let out = execute_skill_in(
            r#"{"name":"pack"}"#,
            &cwd.display().to_string(),
            Some(&xdg.join(atom_core::build::dir_leaf())),
            Some(&hb),
        );
        assert!(out.contains("PACK_INSTRUCTIONS"), "{out}");
        assert!(out.contains(&dir.display().to_string()), "{out}");
        assert!(out.contains("helper.sh"), "{out}");
        assert!(!out.contains("SKILL.md"), "{out}");

        // Unknown name lists known ones.
        let out = execute_skill_in(
            r#"{"name":"nope"}"#,
            &cwd.display().to_string(),
            Some(&xdg.join(atom_core::build::dir_leaf())),
            Some(&hb),
        );
        assert!(out.contains("error: unknown skill \"nope\""), "{out}");
        assert!(out.contains("pack"), "{out}");
    }

    #[test]
    fn catalog_message_lists_names_not_bodies() {
        let (_h, xdg, cwd, hb) = hermetic();
        let long_body = format!("SECRET_SKILL_BODY {}", "x".repeat(200));
        write_skill_md(
            &xdg.join(atom_core::build::dir_leaf()).join("skills").join("longone"),
            "longone",
            "Use for long tasks",
            &long_body,
        );
        let msg = skills_catalog_message_roots(
            &cwd.display().to_string(),
            Some(&xdg.join(atom_core::build::dir_leaf())),
            Some(&hb),
        );
        assert!(msg.contains("Instructions from: skills"), "{msg}");
        assert!(msg.contains("longone") && msg.contains("Use for long tasks"));
        assert!(!msg.contains("SECRET_SKILL_BODY"));
    }

    fn skills_catalog_message_roots(cwd: &str, xdg: Option<&Path>, home: Option<&Path>) -> String {
        let skills = discover_skills_in(cwd, xdg, home);
        if skills.is_empty() {
            return String::new();
        }
        let mut b = String::from("Instructions from: skills\n");
        b.push_str(
            "You can load extra instructions with the skill tool when a skill matches the task. Do not load a skill unless it is relevant.\n",
        );
        for (name, s) in &skills {
            b.push_str(&format!("\n- {name}: {}", s.description));
        }
        b
    }
}
