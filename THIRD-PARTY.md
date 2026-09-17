# Third-party software

Ardur adopts upstream agent surfaces rather than rebuilding every one from
scratch. This file records what is adopted, under which license, and how.

"How" matters for licensing: a **wrapped** component is executed as a separate
process and no upstream source is copied into this repository, so no upstream
license text is embedded. A **ported** component re-implements a published
behavior contract in this repository's own code. A **vendored** component copies
upstream source, and carries its license text alongside the copied file.

---

## Prime Agent

- **License:** MIT
- **Copyright:** (c) 2025 Mario Zechner; (c) 2026 Prime Intellect
- **Upstream:** <https://github.com/PrimeIntellect-ai/prime-agent>

| Component | Relationship | Notes |
| --- | --- | --- |
| `crates/provider-prime` | **Wrapped** (subprocess) | Spawns the `prime-agent` binary in its RPC mode and speaks its published command/event protocol. No upstream source is copied. Protocol observed against prime-agent 0.9.5. |

---

## Hermes Agent

- **License:** MIT
- **Copyright:** (c) Nous Research
- **Upstream:** <https://github.com/NousResearch/hermes-agent>

| Component | Relationship | Notes |
| --- | --- | --- |
| Skill format (`SKILL.md`) | **Format compatibility** | Ardur's skill loader reads the same `SKILL.md` frontmatter + markdown layout, so an existing Hermes skills library can be consumed via `ARDUR_SKILLS_DIRS`. No upstream source is copied. |

---

## Adding an entry

When adopting a new upstream component, record it here with its license,
copyright, upstream URL, and relationship. For a **vendored** file, add an
"adapted from `<url>` @ `<commit-sha>`" note at the top of the file itself and
place the upstream license text beside it. Crate-level `README.md` attribution is
required for every wrapped or ported component.
