//! W6 — compat probe: parse every real Hermes SKILL.md with Ardur's own loader.
//!
//! Ignored by default (it reads a machine-local skills library). Run with:
//! `cargo test -p ardur-tool-registry --test hermes_compat -- --ignored --nocapture`

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use ardur_tool_registry::{Skill, SkillLoader};

/// Every `SKILL.md` under `root`, at any depth.
///
/// Traversal errors are collected rather than swallowed. An unreadable subtree
/// would otherwise shrink the corpus silently, and a probe that certifies
/// compatibility over a corpus it failed to read is worse than no probe at all.
fn find_all_manifests(root: &Path, out: &mut Vec<PathBuf>, errors: &mut Vec<(PathBuf, String)>) {
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(e) => {
            errors.push((root.to_path_buf(), e.to_string()));
            return;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                errors.push((root.to_path_buf(), e.to_string()));
                continue;
            }
        };
        let path = entry.path();
        if path.is_dir() {
            find_all_manifests(&path, out, errors);
        } else if path.file_name().and_then(|n| n.to_str()) == Some("SKILL.md") {
            out.push(path);
        }
    }
}

fn skills_root() -> PathBuf {
    std::env::var("HERMES_SKILLS_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(std::env::var("HOME").expect("HOME")).join(".hermes/skills")
        })
}

#[test]
#[ignore = "reads the machine-local Hermes skills library"]
fn hermes_skill_corpus_parses_and_reports_a_compat_matrix() {
    let root = skills_root();
    assert!(root.is_dir(), "no skills library at {}", root.display());

    // 1. What the CONTENT parser accepts, independent of discovery.
    let mut manifests = Vec::new();
    let mut traversal_errors = Vec::new();
    find_all_manifests(&root, &mut manifests, &mut traversal_errors);
    manifests.sort();

    // Fail BEFORE certifying anything. An empty or partially-unreadable corpus
    // must never pass as "100% compatible" — that is precisely the misconfigured
    // or inaccessible library this probe exists to detect.
    assert!(
        traversal_errors.is_empty(),
        "could not fully traverse {}: {traversal_errors:?}",
        root.display()
    );
    assert!(
        !manifests.is_empty(),
        "no SKILL.md found under {} — refusing to certify compatibility over an empty corpus",
        root.display()
    );

    let mut parsed_ok = 0usize;
    let mut failures: Vec<(PathBuf, String)> = Vec::new();
    for manifest in &manifests {
        match Skill::load_file(manifest) {
            Ok(_) => parsed_ok += 1,
            Err(e) => failures.push((manifest.clone(), e.to_string())),
        }
    }

    // 2. What DISCOVERY actually finds: the loader scans immediate
    //    sub-directories of a skills dir only.
    let discovered_at_root = SkillLoader::load_directory(&root)
        .map(|s| s.len())
        .unwrap_or(0);

    // Depth of each manifest relative to the library root, in directory levels:
    // depth 1 == `<root>/<skill>/SKILL.md` (the layout the loader expects).
    let mut by_depth: BTreeMap<usize, usize> = BTreeMap::new();
    for manifest in &manifests {
        let rel = manifest.strip_prefix(&root).expect("under root");
        by_depth
            .entry(rel.components().count() - 1)
            .and_modify(|c| *c += 1)
            .or_insert(1);
    }

    println!("\n=== HERMES SKILL COMPAT MATRIX ===");
    println!("library root      : {}", root.display());
    println!("SKILL.md found    : {}", manifests.len());
    println!("parsed by Ardur   : {parsed_ok}");
    println!("parse failures    : {}", failures.len());
    println!("discovered by SkillLoader::load_directory(root): {discovered_at_root}");
    println!("\nmanifests by directory depth below the root:");
    for (depth, count) in &by_depth {
        let note = if *depth == 1 {
            "loader finds these"
        } else {
            "INVISIBLE to the loader (nested under a category dir)"
        };
        println!("  depth {depth}: {count:>3}  <- {note}");
    }
    if !failures.is_empty() {
        println!("\nparse failures:");
        for (path, err) in failures.iter().take(20) {
            println!("  {}: {err}", path.display());
        }
    }

    // The real assertion: the content parser is compatible with the Hermes
    // format. Any failure here is a genuine format divergence worth fixing.
    assert!(
        failures.is_empty(),
        "{} Hermes SKILL.md files failed Ardur's parser",
        failures.len()
    );
    // Every manifest found must be accounted for, so the success line cannot be
    // produced by a corpus that was silently truncated mid-walk.
    assert_eq!(
        parsed_ok,
        manifests.len(),
        "parsed count must cover the whole corpus"
    );
}
