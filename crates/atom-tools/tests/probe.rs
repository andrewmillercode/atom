//! Temporary probe to debug why the server sees no skills even though
//! ~/.config/atom-dev/skills/meta-ads/SKILL.md exists on disk.
#[test]
fn probe_real_discovery_at_cwd() {
    let cwd = "/Users/andrewmiller/projects/atom";
    let config = atom_tools::skills::atom_config_dir();
    let home = dirs::home_dir();
    eprintln!("cwd={cwd}");
    eprintln!("config_dir={config:?}");
    eprintln!("home={home:?}");
    eprintln!("XDG_CONFIG_HOME={:?}", std::env::var("XDG_CONFIG_HOME").ok());
    eprintln!(
        "dir_leaf={:?}",
        atom_core::build::dir_leaf()
    );
    eprintln!("is_dev={:?}", atom_core::build::is_dev());
    let skills = atom_tools::skills::discover_skills_in(cwd, config.as_deref(), home.as_deref());
    eprintln!("discovered {} skill(s):", skills.len());
    for (name, s) in &skills {
        eprintln!(
            "  {name}: {} chars body, dir={}",
            s.body.len(),
            s.dir
        );
    }
    let msg = atom_tools::skills::skills_catalog_message(cwd);
    eprintln!("catalog message ({} chars):", msg.len());
    eprintln!("{msg}");
    assert!(true, "intentionally passing — printing is the test");
}
