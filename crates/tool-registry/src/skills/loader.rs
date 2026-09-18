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

/// Environment variable overriding [`MAX_SKILL_DEPTH`].
///
/// A library nested more deeply than the default is an operator setting, not a
/// recompile. An unparseable or zero value falls back to the default with a
/// warning rather than failing the boot, since an unreadable override should
/// not be more disruptive than the misconfiguration it describes.
pub const ARDUR_SKILL_MAX_DEPTH_ENV: &str = "ARDUR_SKILL_MAX_DEPTH";

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
        Self::collect(dir, 0, &mut skills, true, Self::depth_limit())?;

        skills.sort_by(|a, b| a.frontmatter.name.cmp(&b.frontmatter.name));
        Ok(skills)
    }

    /// Resolve the discovery depth bound, honouring the environment override.
    fn depth_limit() -> usize {
        Self::parse_depth_limit(std::env::var(ARDUR_SKILL_MAX_DEPTH_ENV).ok().as_deref())
    }

    /// Interpret a raw override value. Separate from the environment read so
    /// it is testable without mutating a shared process environment.
    fn parse_depth_limit(raw: Option<&str>) -> usize {
        let Some(raw) = raw else {
            return MAX_SKILL_DEPTH;
        };
        match raw.trim().parse::<usize>() {
            Ok(depth) if depth > 0 => depth,
            _ => {
                tracing::warn!(
                    env = ARDUR_SKILL_MAX_DEPTH_ENV,
                    value = %raw,
                    default = MAX_SKILL_DEPTH,
                    "ignoring unusable skills depth override; using the default"
                );
                MAX_SKILL_DEPTH
            }
        }
    }

    /// Load with an explicit depth bound, bypassing the environment override.
    pub fn load_directory_with_depth(
        dir: impl AsRef<Path>,
        depth_limit: usize,
    ) -> Result<Vec<Skill>, SkillError> {
        let dir = dir.as_ref();
        let mut skills = Vec::new();
        Self::collect(dir, 0, &mut skills, true, depth_limit.max(1))?;
        skills.sort_by(|a, b| a.frontmatter.name.cmp(&b.frontmatter.name));
        Ok(skills)
    }

    fn collect(
        dir: &Path,
        depth: usize,
        skills: &mut Vec<Skill>,
        root: bool,
        depth_limit: usize,
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

            // `is_dir()` follows symlinks, so a linked skill directory still
            // loads -- some libraries are assembled by linking, and dropping
            // those would be a silent regression.
            if !path.is_dir() {
                continue;
            }

            // Whether this entry is itself a link decides if we may RECURSE
            // into it. A symlinked directory is loaded as a leaf but never
            // descended into, so a cycle cannot be built out of linked
            // categories and the bounded walk stays bounded.
            let is_symlink = entry.file_type().map(|ft| ft.is_symlink()).unwrap_or(true);

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

            if is_symlink {
                tracing::warn!(
                    path = %path.display(),
                    "not descending into a symlinked directory that holds no SKILL.md; \
                     skills nested below it will not be registered"
                );
                continue;
            }

            if depth + 1 < depth_limit {
                Self::collect(&path, depth + 1, skills, false, depth_limit)?;
            } else {
                // Warn, not debug: a hidden truncation is the same silent
                // partial registration this loader exists to prevent.
                tracing::warn!(
                    path = %path.display(),
                    max_depth = depth_limit,
                    env = ARDUR_SKILL_MAX_DEPTH_ENV,
                    "skills discovery truncated: not descending further, so any \
                     skills below this directory are NOT registered; raise the \
                     depth limit to include them"
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
    fn a_symlinked_skill_directory_still_loads() {
        // Some libraries are assembled by linking skills into a root. The
        // pre-recursion loader used `path.is_dir()`, which follows links, so
        // dropping them here would be a silent regression on upgrade.
        #[cfg(unix)]
        {
            let tmp = tempfile::tempdir().unwrap();
            let store = tmp.path().join("store");
            let root = tmp.path().join("root");
            std::fs::create_dir_all(&root).unwrap();
            write_skill_at(&store, "linked", "linked");
            std::os::unix::fs::symlink(store.join("linked"), root.join("linked")).unwrap();

            let skills = SkillLoader::load_directory(&root).unwrap();
            let names: Vec<_> = skills.iter().map(|s| s.frontmatter.name.as_str()).collect();
            assert_eq!(
                names,
                vec!["linked"],
                "a symlinked skill directory must still be registered"
            );
        }
    }

    #[test]
    fn a_symlink_cycle_terminates_and_does_not_duplicate() {
        // Following links is only safe because a symlinked directory is a
        // LEAF: we never recurse through one. A link pointing at its own
        // ancestor must therefore neither hang nor re-register its contents.
        #[cfg(unix)]
        {
            let tmp = tempfile::tempdir().unwrap();
            write_skill_at(tmp.path(), "category/real", "real");
            // `loop` -> the root itself, placed AT the root and exercised with a
            // raised bound, so the depth limit cannot be what stops the cycle:
            // only the symlinks-are-leaves rule can.
            std::os::unix::fs::symlink(tmp.path(), tmp.path().join("loop")).unwrap();

            let skills = SkillLoader::load_directory_with_depth(tmp.path(), 8).unwrap();
            let names: Vec<_> = skills.iter().map(|s| s.frontmatter.name.as_str()).collect();
            assert_eq!(
                names,
                vec!["real"],
                "a symlink cycle must terminate without duplicate registrations"
            );
        }
    }

    #[test]
    fn the_depth_limit_is_configurable() {
        // A deeper library should be an operator setting, not a recompile.
        let tmp = tempfile::tempdir().unwrap();
        write_skill_at(tmp.path(), "a/b/c/too-deep", "too-deep");

        assert!(
            SkillLoader::load_directory_with_depth(tmp.path(), MAX_SKILL_DEPTH)
                .unwrap()
                .is_empty(),
            "the default bound must exclude this skill, or a raised bound proves nothing"
        );

        let skills = SkillLoader::load_directory_with_depth(tmp.path(), 4).unwrap();
        let names: Vec<_> = skills.iter().map(|s| s.frontmatter.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["too-deep"],
            "a raised bound must admit a deeper tree"
        );
    }

    #[test]
    fn an_unusable_depth_override_falls_back_to_the_default() {
        // An unreadable override must not be more disruptive than the
        // misconfiguration it describes: fall back, warn, keep booting.
        for bad in [Some("not-a-number"), Some("0"), Some(""), Some("-1"), None] {
            assert_eq!(
                SkillLoader::parse_depth_limit(bad),
                MAX_SKILL_DEPTH,
                "override {bad:?} must fall back to the default, not disable discovery"
            );
        }
        assert_eq!(
            SkillLoader::parse_depth_limit(Some(" 5 ")),
            5,
            "a usable override must be honoured, or the fallback test is vacuous"
        );
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
