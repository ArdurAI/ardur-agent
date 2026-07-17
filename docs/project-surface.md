# Kanban and multi-agent project surface

`ardur project` stores a lightweight local coordination surface in `~/.ardur/project-surface.json`.

It has two append-friendly sections:

- `cards`: Kanban cards with id, title, status, owner, and timestamps.
- `runs`: multi-agent run ledger entries with agent, summary, receipt evidence, optional related card, status, and timestamp.

Commands:

```sh
ardur project board
ardur project add-card "Implement signed marketplace" --status ready --owner codex-worker
ardur project move <card-id> in-review
ardur project record-run --agent codex-worker --summary "implemented + tested" --receipt https://github.com/ArdurAI/ardur-agent/pull/232 --card <card-id> --status completed
```

`record-run --card` refuses unknown card ids, so ledger evidence remains attached to a real board item.
