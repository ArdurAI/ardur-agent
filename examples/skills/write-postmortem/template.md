# Postmortem template

Copy and fill. Keep it blameless throughout — roles, not names (except to credit
good work).

```markdown
# Postmortem: <short incident name>

**Date of incident:** <YYYY-MM-DD>  ·  **Authors:** <names>  ·  **Status:** draft / reviewed / closed
**Severity:** <SEV level>  ·  **Time to detect:** <duration>  ·  **Time to resolve:** <duration>

## Summary
<2-4 sentences: what broke, who it affected, how long, how it was resolved.>

## Impact
- User-facing: <what users could not do>
- Scope: <how many users / regions / tenants / % of traffic>
- Quantified: <requests failed, revenue, error budget, data affected>

## Timeline (all times <TZ>)
- <HH:MM> — <setup condition / change deployed>
- <HH:MM> — <incident begins>
- <HH:MM> — <detected: how>
- <HH:MM> — <acknowledged / on-call engaged>
- <HH:MM> — <key diagnosis / mitigation step>
- <HH:MM> — <resolved>

## Contributing factors
- Trigger: <what changed / what happened>
- Vulnerability: <why the system was susceptible>
- Why not caught earlier: <testing / review / staging gap>
- Why detection was slow: <alerting gap>
- Why mitigation was slow: <missing runbook / ownership / rollback risk>

## What went well
- <existing defense that limited damage>
- <fast/effective action worth replicating>

## Action items
| Action | Owner | Due | Prevents recurrence of |
|--------|-------|-----|------------------------|
| <specific, verifiable change> | <name> | <date> | <class of failure> |
| <...> | <name> | <date> | <...> |

## Lessons
<What the org should take away beyond the action items.>
```
