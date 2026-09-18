//! [`SkillLoader`] — discover and parse every [`Skill`] under a directory.
//!
//! A *skills directory* is a collection of skill sub-directories, each holding a
//! `SKILL.md`:
//!
//! ```text
//! skills/
//!   git-commit-message/
//!     SKILL.md
//!     resource.md        # referenced via @./resource.md
//!   code-review/
//!     SKILL.md
//! ```
//!
//! Libraries that group skills under category directories are also supported:
//!
//! ```text
//! skills/
//!   github/                # a category, NOT a skill (no SKILL.md)
//!     github-pr-workflow/
//!       SKILL.md
//!   git-commit-message/    # a skill at the top level
//!     SKILL.md
//! ```
//!
//! Discovery recurses to [`MAX_SKILL_DEPTH`] so both layouts load from a single
//! `ARDUR_SKILLS_DIRS` entry. Scanning only immediate sub-directories imported 3
//! of 134 skills from a real category-organised library, silently — a directory
//! without a `SKILL.md` was simply skipped, so the operator saw a working boot
//! with almost nothing registered (gh#506).
//!
//! The boot path passes one or more such directories (the comma-separated
//! `ARDUR_SKILLS_DIRS`) and registers a
//! [`SkillTool`](crate::SkillTool) per discovered skill.

use std::path::Path;

use crate::skills::skill::{Skill, SkillError};

/// How many directory levels below a skills root are searched.
///
/// Bounded rather than unlimited: a skills root is operator-supplied, and an
/// unbounded walk would follow an arbitrarily deep tree (or a symlink loop)
/// during boot. Three levels covers a `library/category/skill` layout with one
/// level spare; anything deeper is a filesystem to index, not a skills library.
pub const MAX_SKILL_DEPTH: usize = 3;

/// Loads [`Skill`]s from the filesystem.
pub struct SkillLoader;

impl SkillLoader {
    /// Load every skill under `dir`: one per immediate sub-directory that
    /// contains a readable, valid `SKILL.md`. The result is sorted by skill name
    /// for a deterministic registration order.
    ///
    /// A sub-directory whose `SKILL.md` fails to parse is logged at `warn` and
    /// skipped — one malformed skill never blocks the rest from loading.
    ///
    /// # Errors
    ///
    /// [`SkillError::Io`] if `dir` itself cannot be read (e.g. it does not
    /// exist). Per-skill read/parse failures are warned-and-skipped, not
    /// propagated.
    pub fn load_directory(dir: impl AsRef<Path>) -> Result<Vec<Skill>, SkillError> {
        let dir = dir.as_ref();
        let mut skills = Vec::new();

        // The root is read eagerly so a missing or unreadable root stays a hard
        // error. Only nested levels are best-effort.
        Self::collect(dir, 0, &mut skills, true)?;

        skills.sort_by(|a, b| a.frontmatter.name.cmp(&b.frontmatter.name));
        Ok(skills)
    }

    fn collect(
        dir: &Path,
        depth: usize,
        skills: &mut Vec<Skill>,
        root: bool,
    ) -> Result<(), SkillError> {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if root => return Err(SkillError::Io(error)),
            Err(error) => {
                tracing::warn!(
                    path = %dir.display(),
                    %error,
                    "skipping unreadable skills sub-directory"
                );
                return Ok(());
            }
        };

        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if root => return Err(SkillError::Io(error)),
                Err(error) => {
                    tracing::warn!(path = %dir.display(), %error, "skipping unreadable entry");
                    continue;
                }
            };
            let path = entry.path();

            // `file_type()` does not follow symlinks, so a link to a directory
            // is not descended into. That is what keeps the bounded walk
            // bounded: a symlink cycle would otherwise present the same
            // directory at every level and defeat the depth limit.
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if !file_type.is_dir() {
                continue;
            }

            let manifest = path.join("SKILL.md");
            if manifest.is_file() {
                match Skill::load_file(&manifest) {
                    Ok(skill) => skills.push(skill),
                    Err(error) => tracing::warn!(
                        path = %manifest.display(),
                        %error,
                        "skipping invalid SKILL.md"
                    ),
                }
                // A skill directory is a leaf: its other files are resources.
                continue;
            }

            if depth + 1 < MAX_SKILL_DEPTH {
                Self::collect(&path, depth + 1, skills, false)?;
            } else {
                tracing::debug!(
                    path = %path.display(),
                    max_depth = MAX_SKILL_DEPTH,
                    "not descending further for skills"
                );
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stage `<root>/<name>/SKILL.md` with the given contents.
    fn write_skill(root: &Path, name: &str, contents: &str) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("SKILL.md"), contents).unwrap();
    }

    /// Stage `<root>/<rel>/SKILL.md`, creating intermediate directories.
    fn write_skill_at(root: &Path, rel: &str, name: &str) {
        let dir = root.join(rel);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: A {name}.\n---\nbody"),
        )
        .unwrap();
    }

    #[test]
    fn finds_skills_nested_under_category_directories() {
        // The gh#506 case: a library grouping skills by category imported 3 of
        // 134 because only immediate sub-directories were scanned.
        let tmp = tempfile::tempdir().unwrap();
        write_skill_at(tmp.path(), "top-level", "top-level");
        write_skill_at(tmp.path(), "github/pr-workflow", "pr-workflow");
        write_skill_at(tmp.path(), "github/issues", "issues");
        write_skill_at(tmp.path(), "mlops/training/axolotl", "axolotl");

        let skills = SkillLoader::load_directory(tmp.path()).unwrap();
        let names: Vec<_> = skills.iter().map(|s| s.frontmatter.name.as_str()).collect();

        assert_eq!(
            names,
            vec!["axolotl", "issues", "pr-workflow", "top-level"],
            "flat and nested skills must both load, sorted by name"
        );
    }

    #[test]
    fn a_skill_directory_is_a_leaf_and_is_not_descended_into() {
        // Files beside a manifest are the skill's resources. Treating a nested
        // directory as another skill would register a phantom, and a resource
        // directory that happened to contain SKILL.md would shadow its parent.
        let tmp = tempfile::tempdir().unwrap();
        write_skill_at(tmp.path(), "parent", "parent");
        write_skill_at(tmp.path(), "parent/references", "nested-should-not-load");

        let skills = SkillLoader::load_directory(tmp.path()).unwrap();
        let names: Vec<_> = skills.iter().map(|s| s.frontmatter.name.as_str()).collect();

        assert_eq!(
            names,
            vec!["parent"],
            "a directory holding SKILL.md is a skill, not a category"
        );
    }

    #[test]
    fn discovery_stops_at_the_depth_limit() {
        // The bound is the reason an operator-supplied root cannot turn boot
        // into a full filesystem walk. Prove it actually stops.
        let tmp = tempfile::tempdir().unwrap();
        write_skill_at(tmp.path(), "a/b/deep", "deep");
        write_skill_at(tmp.path(), "a/b/c/too-deep", "too-deep");

        let skills = SkillLoader::load_directory(tmp.path()).unwrap();
        let names: Vec<_> = skills.iter().map(|s| s.frontmatter.name.as_str()).collect();

        assert_eq!(
            names,
            vec!["deep"],
            "MAX_SKILL_DEPTH={MAX_SKILL_DEPTH} must admit a/b/deep and exclude a/b/c/too-deep"
        );
    }

    #[test]
    fn a_missing_root_is_still_a_hard_error() {
        // Nested failures are warned-and-skipped, but a root that does not
        // exist is operator misconfiguration and must not boot silently empty.
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");

        assert!(
            SkillLoader::load_directory(&missing).is_err(),
            "an unreadable ROOT must fail loudly, not return zero skills"
        );
    }

    #[test]
    fn a_symlinked_directory_is_not_followed() {
        // `file_type()` does not follow symlinks, which is what keeps the
        // bounded walk bounded: a cycle would otherwise present the same
        // directory at every level and defeat the depth limit.
        #[cfg(unix)]
        {
            let tmp = tempfile::tempdir().unwrap();
            write_skill_at(tmp.path(), "real", "real");
            let linked = tmp.path().join("link");
            std::os::unix::fs::symlink(tmp.path().join("real"), &linked).unwrap();

            let skills = SkillLoader::load_directory(tmp.path()).unwrap();
            let names: Vec<_> = skills.iter().map(|s| s.frontmatter.name.as_str()).collect();
            assert_eq!(
                names,
                vec!["real"],
                "a symlink must not re-register the skill it points at"
            );
        }
    }

    #[test]
    fn an_empty_category_contributes_nothing_and_does_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill_at(tmp.path(), "github/pr-workflow", "pr-workflow");
        std::fs::create_dir_all(tmp.path().join("empty-category")).unwrap();

        let skills = SkillLoader::load_directory(tmp.path()).unwrap();
        assert_eq!(skills.len(), 1, "an empty category is skipped, not fatal");
    }

    #[test]
    fn loads_minimal() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "demo",
            "---\nname: demo\ndescription: A demo.\n---\nbody",
        );

        let skills = SkillLoader::load_directory(tmp.path()).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].frontmatter.name, "demo");
        assert_eq!(skills[0].dir, tmp.path().join("demo"));
    }

    #[test]
    fn loads_multiple_sorted_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        write_skill(
            tmp.path(),
            "zeta",
            "---\nname: zeta\ndescription: z.\n---\nz",
        );
        write_skill(
            tmp.path(),
            "alpha",
            "---\nname: alpha\ndescription: a.\n---\na",
        );

        let skills = SkillLoader::load_directory(tmp.path()).unwrap();
        let names: Vec<_> = skills.iter().map(|s| s.frontmatter.name.as_str()).collect();
        assert_eq!(names, ["alpha", "zeta"]);
    }

    #[test]
    fn skips_invalid_and_keeps_valid() {
        let tmp = tempfile::tempdir().unwrap();
        // Missing `description` — invalid, must be warned-and-skipped.
        write_skill(tmp.path(), "broken", "---\nname: broken\n---\nbody");
        write_skill(
            tmp.path(),
            "ok",
            "---\nname: ok\ndescription: d.\n---\nbody",
        );
        // A non-skill sub-directory (no SKILL.md) is ignored.
        std::fs::create_dir_all(tmp.path().join("not-a-skill")).unwrap();

        let skills = SkillLoader::load_directory(tmp.path()).unwrap();
        assert_eq!(skills.len(), 1);
        assert_eq!(skills[0].frontmatter.name, "ok");
    }

    #[test]
    fn missing_directory_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("does-not-exist");
        assert!(matches!(
            SkillLoader::load_directory(&missing),
            Err(SkillError::Io(_))
        ));
    }
}
