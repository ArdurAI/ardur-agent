//! [`Skill`] — a filesystem skill parsed from a `SKILL.md` document, plus its
//! [`SkillFrontmatter`] and the [`SkillError`] surfaced when a document is
//! malformed.
//!
//! A `SKILL.md` is a YAML frontmatter block delimited by `---` lines, followed
//! by a Markdown body:
//!
//! ```text
//! ---
//! name: git-commit-message
//! description: Draft a Conventional-Commits message for a staged diff.
//! metadata:
//!   category: git
//! ---
//! # How to write the commit message
//! ...the body the model receives when it invokes the skill...
//! ```
//!
//! The body may reference sibling files with `@./resource.md` markers for
//! *progressive disclosure*: they stay un-inlined (cheap) until a caller asks
//! for them — see [`SkillTool`](crate::SkillTool).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The parsed YAML frontmatter of a `SKILL.md` document.
///
/// `name` and `description` are required; everything under `metadata` is an
/// open, free-form map. Unknown top-level fields are ignored (serde does not
/// `deny_unknown_fields`) so a newer `SKILL.md` schema stays loadable by an
/// older binary — forward-compatibility by construction.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
pub struct SkillFrontmatter {
    /// The skill's stable name. Becomes the registered tool id.
    pub name: String,
    /// One line describing what the skill does. Becomes the tool's description.
    pub description: String,
    /// Free-form, author-defined metadata. Empty when the key is absent.
    #[serde(default)]
    pub metadata: serde_json::Map<String, serde_json::Value>,
}

/// A skill loaded from a `SKILL.md` file: its [`SkillFrontmatter`], its Markdown
/// `body`, and the `dir` the file lives in (the base for `@./` resource refs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skill {
    /// The parsed YAML frontmatter.
    pub frontmatter: SkillFrontmatter,
    /// The Markdown body that follows the frontmatter, trimmed of surrounding
    /// blank lines.
    pub body: String,
    /// The directory the `SKILL.md` was read from; `@./file` resource references
    /// resolve relative to it.
    pub dir: PathBuf,
}

impl Skill {
    /// Parse a `SKILL.md` document from `content`, recording `dir` as its base
    /// directory for resource resolution.
    ///
    /// # Errors
    ///
    /// - [`SkillError::MissingFrontmatter`] if `content` is not introduced by a
    ///   `---` frontmatter block.
    /// - [`SkillError::InvalidFrontmatter`] if the frontmatter is not valid YAML
    ///   or omits a required field.
    /// - [`SkillError::EmptyField`] if `name` or `description` is present but
    ///   blank.
    pub fn parse(content: &str, dir: impl Into<PathBuf>) -> Result<Self, SkillError> {
        let (yaml, body) = split_frontmatter(content)?;
        let frontmatter: SkillFrontmatter = serde_yaml::from_str(&yaml)?;
        if frontmatter.name.trim().is_empty() {
            return Err(SkillError::EmptyField("name"));
        }
        if frontmatter.description.trim().is_empty() {
            return Err(SkillError::EmptyField("description"));
        }
        Ok(Self {
            frontmatter,
            body: body.trim().to_string(),
            dir: dir.into(),
        })
    }

    /// Read and parse a `SKILL.md` from `path`. The file's parent directory is
    /// recorded as the [`dir`](Skill::dir) for resource resolution.
    ///
    /// # Errors
    ///
    /// [`SkillError::Io`] if the file cannot be read, or any parse error from
    /// [`Skill::parse`].
    pub fn load_file(path: impl AsRef<Path>) -> Result<Self, SkillError> {
        let path = path.as_ref();
        let content = std::fs::read_to_string(path)?;
        let dir = path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        Self::parse(&content, dir)
    }
}

/// Split a `SKILL.md` document into its raw YAML frontmatter and its Markdown
/// body. The document must open with a `---` line; the frontmatter runs to the
/// next standalone `---` line.
fn split_frontmatter(content: &str) -> Result<(String, String), SkillError> {
    // Tolerate a UTF-8 BOM at the very start of the file.
    let content = content.strip_prefix('\u{feff}').unwrap_or(content);
    let mut lines = content.lines();

    match lines.next() {
        Some(line) if line.trim_end() == "---" => {}
        _ => return Err(SkillError::MissingFrontmatter),
    }

    let mut yaml = String::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if line.trim_end() == "---" {
            closed = true;
            break;
        }
        yaml.push_str(line);
        yaml.push('\n');
    }
    if !closed {
        return Err(SkillError::MissingFrontmatter);
    }

    let body = lines.collect::<Vec<_>>().join("\n");
    Ok((yaml, body))
}

/// Every way loading a [`Skill`] from a `SKILL.md` can fail.
#[derive(Debug, thiserror::Error)]
pub enum SkillError {
    /// The document did not open with a `---`-delimited YAML frontmatter block.
    #[error("SKILL.md is missing its `---`-delimited YAML frontmatter block")]
    MissingFrontmatter,

    /// The frontmatter was not valid YAML, or a required field was absent.
    #[error("SKILL.md frontmatter is invalid or missing a required field: {0}")]
    InvalidFrontmatter(#[from] serde_yaml::Error),

    /// A required field (`name` or `description`) was present but blank.
    #[error("SKILL.md frontmatter field `{0}` is present but empty")]
    EmptyField(&'static str),

    /// The `SKILL.md` file could not be read.
    #[error("reading SKILL.md failed: {0}")]
    Io(#[from] std::io::Error),
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const MINIMAL: &str = "---\nname: demo\ndescription: A demo skill.\n---\nDo the thing.\n";

    #[test]
    fn parses_minimal() {
        let skill = Skill::parse(MINIMAL, "/skills/demo").expect("minimal SKILL.md parses");
        assert_eq!(skill.frontmatter.name, "demo");
        assert_eq!(skill.frontmatter.description, "A demo skill.");
        assert_eq!(skill.body, "Do the thing.");
        assert_eq!(skill.dir, PathBuf::from("/skills/demo"));
        assert!(skill.frontmatter.metadata.is_empty());
    }

    #[test]
    fn rejects_missing_name() {
        let src = "---\ndescription: no name here.\n---\nbody";
        assert!(matches!(
            Skill::parse(src, "."),
            Err(SkillError::InvalidFrontmatter(_))
        ));
    }

    #[test]
    fn rejects_missing_description() {
        let src = "---\nname: lonely\n---\nbody";
        assert!(matches!(
            Skill::parse(src, "."),
            Err(SkillError::InvalidFrontmatter(_))
        ));
    }

    #[test]
    fn rejects_blank_name() {
        let src = "---\nname: \"  \"\ndescription: d\n---\nbody";
        assert!(matches!(
            Skill::parse(src, "."),
            Err(SkillError::EmptyField("name"))
        ));
    }

    #[test]
    fn rejects_missing_frontmatter() {
        let src = "no fences here\njust markdown";
        assert!(matches!(
            Skill::parse(src, "."),
            Err(SkillError::MissingFrontmatter)
        ));
    }

    #[test]
    fn rejects_unterminated_frontmatter() {
        let src = "---\nname: demo\ndescription: d\nbody never closes";
        assert!(matches!(
            Skill::parse(src, "."),
            Err(SkillError::MissingFrontmatter)
        ));
    }

    #[test]
    fn parses_metadata() {
        let src =
            "---\nname: m\ndescription: d\nmetadata:\n  category: git\n  version: 2\n---\nbody";
        let skill = Skill::parse(src, ".").expect("metadata parses");
        assert_eq!(skill.frontmatter.metadata["category"], json!("git"));
        assert_eq!(skill.frontmatter.metadata["version"], json!(2));
    }

    #[test]
    fn unknown_fields_ignored() {
        // A field the current schema does not know about must not break parsing —
        // forward-compatibility for a newer SKILL.md schema.
        let src =
            "---\nname: u\ndescription: d\nfuture_field: whatever\nnested:\n  a: 1\n---\nbody";
        let skill = Skill::parse(src, ".").expect("unknown fields are ignored");
        assert_eq!(skill.frontmatter.name, "u");
        assert!(skill.frontmatter.metadata.is_empty());
    }
}
