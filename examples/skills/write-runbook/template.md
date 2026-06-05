# Runbook template

Copy this skeleton and fill every section. Delete a heading only if it genuinely
does not apply — and say why.

```markdown
# Runbook: <short failure name>

**Owner:** <team / rotation>  ·  **Last verified:** <YYYY-MM-DD>  ·  **Severity:** <SEV level>

## Symptom
- Alert: <exact alert name / text>
- Dashboard: <panel name, threshold crossed>
- User-visible: <what a user experiences>

## Impact
- Scope: <all users / one region / one tenant>
- Degradation: <down vs degraded; data-loss risk?>

## Diagnosis (confirm this is the right runbook)
1. <command>
   - Expected (healthy): <value>
   - This runbook applies when: <condition>
2. <command to rule out the common look-alike>

## Mitigation (stop the bleeding, least-risky first)
1. <exact command with <placeholders>>
   - Effect: <what it does>  ·  Time to take effect: <duration>
   - Reversible? <yes/no — if no, what it costs>
2. <next action if step 1 is insufficient>

## Verification (confirm service restored)
1. <command> -> expect <healthy value>
2. User-facing check: <how a real request looks now>

## Escalation
- If mitigation fails or symptom does not match: <next step>
- Page: <role / rotation, not a person>
- Capture before escalating: <logs, diagnosis output, what was tried>

## Root cause & follow-up
- Cause (if known): <one line>
- Permanent fix: <link to issue / postmortem>
- Retire this runbook when: <condition>
```
