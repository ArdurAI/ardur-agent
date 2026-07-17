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
//! The boot path passes one or more such directories (the comma-separated
//! `ARDUR_SKILLS_DIRS`) and registers a
//! [`SkillTool`](crate::SkillTool) per discovered skill.

use std::path::Path;

use crate::skills::skill::{Skill, SkillError};

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

        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let manifest = path.join("SKILL.md");
            if !manifest.is_file() {
                continue;
            }
            match Skill::load_file(&manifest) {
                Ok(skill) => skills.push(skill),
                Err(error) => tracing::warn!(
                    path = %manifest.display(),
                    %error,
                    "skipping invalid SKILL.md"
                ),
            }
        }

        skills.sort_by(|a, b| a.frontmatter.name.cmp(&b.frontmatter.name));
        Ok(skills)
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
