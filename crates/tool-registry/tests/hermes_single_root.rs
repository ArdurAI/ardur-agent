//! Measures what a single `ARDUR_SKILLS_DIRS` entry actually imports (gh#506).
//!
//! The existing `hermes_categories` probe loads each category directory
//! separately — a workaround that proved the parser was fine, not that
//! discovery was. This measures the thing an operator actually does: point one
//! root at a category-organised library and boot.
//!
//! `#[ignore]`d: it depends on a real skills library outside the repo, so CI
//! must not require it.

use std::path::PathBuf;

use ardur_tool_registry::{MAX_SKILL_DEPTH, SkillLoader};

/// The library root, overridable so this is not tied to one machine.
fn library_root() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("ARDUR_HERMES_SKILLS") {
        let path = PathBuf::from(dir);
        return path.is_dir().then_some(path);
    }
    let home = std::env::var("HOME").ok()?;
    let path = PathBuf::from(home).join(".hermes/skills");
    path.is_dir().then_some(path)
}

/// Count manifests on disk within reach of the loader's depth bound.
fn manifests_within_depth(root: &std::path::Path) -> usize {
    fn walk(dir: &std::path::Path, depth: usize, found: &mut usize) {
        if depth >= MAX_SKILL_DEPTH {
            return;
        }
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(ft) = entry.file_type() else { continue };
            if !ft.is_dir() {
                continue;
            }
            if path.join("SKILL.md").is_file() {
                *found += 1;
                // A skill directory is a leaf, matching the loader.
                continue;
            }
            walk(&path, depth + 1, found);
        }
    }
    let mut found = 0;
    walk(root, 0, &mut found);
    found
}

#[test]
#[ignore = "requires a real skills library; run with --ignored"]
fn a_single_root_imports_the_whole_reachable_library() {
    let Some(root) = library_root() else {
        panic!(
            "no skills library found. Set ARDUR_HERMES_SKILLS to one, or place it \
             at ~/.hermes/skills. Skipping silently would let this probe 'pass' \
             while measuring nothing."
        );
    };

    let on_disk = manifests_within_depth(&root);
    assert!(
        on_disk > 0,
        "no manifests found under {} within depth {MAX_SKILL_DEPTH}; the corpus \
         is empty, so any load count would be vacuously correct",
        root.display()
    );

    let loaded = SkillLoader::load_directory(&root).expect("the library root loads");

    println!("library root      : {}", root.display());
    println!("manifests on disk : {on_disk} (within depth {MAX_SKILL_DEPTH})");
    println!("skills loaded     : {}", loaded.len());

    assert_eq!(
        loaded.len(),
        on_disk,
        "a single root must import every manifest within the depth bound; \
         this is the gh#506 regression (it imported 3 of 134 before the fix)"
    );

    // Names must be unique, or registration silently drops the collisions and
    // the count above overstates what a runtime would actually have.
    let mut names: Vec<_> = loaded.iter().map(|s| s.frontmatter.name.clone()).collect();
    names.sort();
    let total = names.len();
    names.dedup();
    assert_eq!(
        names.len(),
        total,
        "duplicate skill names would be dropped at registration, so the load \
         count would not reflect the registered set"
    );
}
