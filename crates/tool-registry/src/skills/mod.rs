//! `skills` — load filesystem **SKILL.md** documents and expose each as a
//! [`Tool`](crate::Tool).
//!
//! A *skill* is a directory holding a `SKILL.md`: YAML frontmatter (`name`,
//! `description`, optional free-form `metadata`) followed by a Markdown body.
//! The body is the instruction text the model receives when it invokes the
//! skill; it may reference sibling files with `@./resource.md` markers for
//! *progressive disclosure* — those resources stay un-inlined until a caller
//! asks for them.
//!
//! # Pieces
//!
//! - [`Skill`] / [`SkillFrontmatter`] — a parsed `SKILL.md` (frontmatter, body,
//!   and source directory) and its required + free-form fields.
//! - [`SkillLoader`] — discovers every `<name>/SKILL.md` under a directory and
//!   parses it, warning-and-skipping malformed ones.
//! - [`SkillTool`] — adapts a [`Skill`] to the [`Tool`](crate::Tool) contract:
//!   id = `name`, description = `description`, and `invoke` returns the body. Its
//!   optional `expand: string[]` argument inlines named `@./` resources.
//! - [`SkillError`] — the load/parse failure surface.
//!
//! # Wiring
//!
//! The server reads a comma-separated `ARDUR_SKILLS_DIRS`, calls
//! [`SkillLoader::load_directory`] on each, and registers a [`SkillTool`] per
//! discovered skill into the same [`ToolRegistry`](crate::ToolRegistry) the
//! runtime invokes — so skills sit alongside compiled-in and remote MCP tools.
//!
//! # Validation
//!
//! `name` and `description` are required; a `SKILL.md` missing either is
//! rejected (and, at the directory level, skipped with a warning). Unknown
//! frontmatter fields are ignored so a newer schema stays loadable by an older
//! binary.

mod loader;
mod skill;
mod tool;

pub use loader::SkillLoader;
pub use skill::{Skill, SkillError, SkillFrontmatter};
pub use tool::SkillTool;
