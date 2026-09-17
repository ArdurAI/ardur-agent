//! W6 — prove the category-dir workaround actually loads real Hermes skills.

use std::path::PathBuf;

use ardur_tool_registry::SkillLoader;

fn skills_root() -> PathBuf {
    std::env::var("HERMES_SKILLS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").expect("HOME")).join(".hermes/skills")
        })
}

/// Category directories: immediate children of the library root that themselves
/// contain skill sub-directories. This is what `ARDUR_SKILLS_DIRS` must be
/// pointed at, one entry per category, for a nested library.
fn category_dirs(root: &PathBuf) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let Ok(entries) = std::fs::read_dir(root) else {
        return out;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let has_nested_skill = std::fs::read_dir(&path)
            .map(|children| {
                children
                    .flatten()
                    .any(|c| c.path().is_dir() && c.path().join("SKILL.md").is_file())
            })
            .unwrap_or(false);
        if has_nested_skill {
            out.push(path);
        }
    }
    out.sort();
    out
}

#[test]
#[ignore = "reads the machine-local Hermes skills library"]
fn category_dirs_load_real_skills_through_the_unmodified_loader() {
    let root = skills_root();
    assert!(root.is_dir(), "no skills library at {}", root.display());

    let cats = category_dirs(&root);
    assert!(!cats.is_empty(), "expected category directories");

    let mut loaded = 0usize;
    let mut names: Vec<String> = Vec::new();
    for cat in &cats {
        let skills = SkillLoader::load_directory(cat)
            .unwrap_or_else(|e| panic!("loading {}: {e}", cat.display()));
        loaded += skills.len();
        names.extend(skills.iter().map(|s| s.frontmatter.name.clone()));
    }

    println!("\n=== CATEGORY-DIR WORKAROUND ===");
    println!("category dirs        : {}", cats.len());
    println!("skills loaded        : {loaded}");
    println!("sample names         : {:?}", &names[..names.len().min(8)]);

    // Every loaded skill must carry the two required fields non-empty — that is
    // what makes it registrable as a tool.
    assert!(loaded > 0, "no skills loaded from category dirs");
    assert!(
        names.iter().all(|n| !n.trim().is_empty()),
        "a loaded skill had an empty name"
    );

    // Duplicate names across categories would collide on tool registration —
    // report rather than assume uniqueness.
    let mut sorted = names.clone();
    sorted.sort();
    let unique = {
        let mut s = sorted.clone();
        s.dedup();
        s.len()
    };
    println!("unique names         : {unique} of {loaded}");
    if unique != loaded {
        let mut dupes: Vec<&String> = Vec::new();
        for w in sorted.windows(2) {
            if w[0] == w[1] && !dupes.contains(&&w[0]) {
                dupes.push(&w[0]);
            }
        }
        println!("DUPLICATE names (would collide on registration): {dupes:?}");
    }
}
