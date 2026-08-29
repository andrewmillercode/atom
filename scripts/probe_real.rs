fn main() {
    let args: Vec<String> = std::env::args().collect();
    let cwd = args.get(1).cloned().unwrap_or_else(|| "/Users/andrewmiller/projects/atom".to_string());
    let skills = atom_tools_rs::skills::discover_skills(&cwd);
    println!("cwd={cwd}");
    for (name, s) in &skills {
        println!("  {name}: {} chars body, dir={}", s.body.len(), s.dir);
    }
    println!("count={}", skills.len());
    println!("config_dir={:?}", atom_tools_rs::skills::atom_config_dir());
}
