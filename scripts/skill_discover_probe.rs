// Test harness: exercises discover_skills exactly as atom-tools does.
use std::collections::BTreeMap;
use std::path::Path;

fn load_skills_from_dir(root: &Path, into: &mut BTreeMap<String, String>) {
    let entries = match std::fs::read_dir(root) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("  read_dir({}) ERR: {e}", root.display());
            return;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path().join("SKILL.md");
        let b = match std::fs::read(&path) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("  read({}) ERR: {e}", path.display());
                continue;
            }
        };
        let dir = entry.path();
        let name = dir
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        into.insert(name, format!("{} bytes @ {}", b.len(), dir.display()));
    }
}

fn main() {
    let mut out = BTreeMap::new();
    let cwd = "/Users/andrewmiller/projects/atom";
    let home = std::env::home_dir();
    let config = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(|| home.clone().map(|h| h.join(".config")));

    println!("home={:?}", home);
    println!("config={:?}", config);
    println!("cwd={}", cwd);
    println!("XDG_CONFIG_HOME={:?}", std::env::var("XDG_CONFIG_HOME").ok());

    if let Some(c) = config.clone() {
        let d = c.join("atom-dev/skills");
        println!("load {}", d.display());
        load_skills_from_dir(&d, &mut out);
    }
    println!("after config: {:?}", out.keys().collect::<Vec<_>>());

    if let Some(h) = home.clone() {
        for sub in [".agents/skills", ".cursor/skills"] {
            let d = h.join(sub);
            println!("load {}", d.display());
            load_skills_from_dir(&d, &mut out);
        }
    }
    println!("after home: {:?}", out.keys().collect::<Vec<_>>());

    let cwd_pb = std::path::PathBuf::from(cwd);
    let inside_home = home.as_ref().map(|h| cwd_pb.starts_with(h)).unwrap_or(false);
    println!("inside_home={}", inside_home);
    let mut dirs = Vec::new();
    let mut cur = cwd_pb;
    loop {
        dirs.push(cur.clone());
        let stop_at_root = !inside_home && cur.parent().is_none();
        let at_home = home.as_deref().map(|h| cur == h).unwrap_or(false);
        let at_root = cur == std::path::Path::new("/");
        if at_home || stop_at_root || (at_root && inside_home) {
            break;
        }
        let parent = match cur.parent() {
            Some(p) if p != cur => p.to_path_buf(),
            _ => break,
        };
        cur = parent;
    }
    println!("project dirs in order: {:?}", dirs);
    for d in dirs.iter().rev() {
        for sub in [".atom/skills", ".cursor/skills", ".agents/skills"] {
            let p = d.join(sub);
            println!("load {}", p.display());
            load_skills_from_dir(&p, &mut out);
        }
    }
    println!("FINAL: {:?}", out);
}
