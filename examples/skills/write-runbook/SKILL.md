---
name: write-runbook
description: Turn an incident response into a reusable runbook the next on-call can follow.
metadata:
  type: incident
  version: 1
---
# Writing a runbook

A runbook turns a once-solved problem into a procedure anyone on-call can
execute at 3am, half-asleep, without your context. Write it for that reader: not
the expert who already knows the system, but the tired engineer who has never
seen this failure before.

The test of a good runbook is simple — could someone who has never touched this
service resolve the incident by following it literally? If a step requires
judgement you have not written down, the runbook is not done.

## When to write one

Write a runbook when a problem is **recurring or likely to recur**, has a
**known response**, and is **time-sensitive** when it happens. A one-off that
will never repeat does not need a runbook; it needs a postmortem. A problem with
no known response does not need a runbook yet; it needs investigation.

## Structure

Follow this order. It matches the order an on-call engineer actually needs the
information. The full skeleton with prompts is in `@./template.md`.

### 1. Symptom — how you know you are here

What does the operator observe? Be concrete and matchable:

- The exact alert name or alert text that fires.
- The dashboard panel that goes red and its threshold.
- The user-visible behavior ("checkout returns 503", "search latency > 2s").

The operator arrives holding a symptom, not a diagnosis. They must be able to
match what they see against your symptom line in seconds. If two failures look
identical at the symptom level, say so and point to the disambiguating check.

### 2. Impact — how bad, how urgent

- Who is affected and how many (all users, one region, one tenant)?
- What is degraded versus fully down?
- Is there data loss risk, or only availability?

This sets the operator's urgency and tells them whether to page for help before
proceeding.

### 3. Diagnosis — confirm before you act

Steps to confirm this is the failure the runbook addresses, and not a look-alike.
Each step is a concrete command or query with the expected output:

```
# Is the queue actually backed up, or is the metric stale?
$ redis-cli -h <host> LLEN jobs:pending
# Expected when healthy: < 100. If > 10000, this runbook applies.
```

Diagnosis steps prevent the operator from applying a mitigation to the wrong
problem. Always include a check that *rules the failure in*, not just one that
looks consistent with it.

### 4. Mitigation — stop the bleeding

The actions that restore service, in order, least-risky first.

- Give the **exact** command, with placeholders clearly marked (`<host>`,
  `<region>`). The operator should not have to compose a command under pressure.
- State the expected effect and how long it takes.
- Flag anything destructive or irreversible **before** the command, and say what
  it costs.
- Prefer reversible mitigations (scale up, failover, feature-flag off) over
  irreversible ones (delete, truncate). Restoring service comes before fixing
  the cause.

### 5. Verification — confirm it worked

How the operator knows service is restored. The same checks as diagnosis, now
expecting healthy values. Include the user-facing signal, not just the internal
metric — the metric can recover while users are still broken.

### 6. Escalation — when this runbook is not enough

- What to do if mitigation does not work or the symptom does not match.
- Who to page (role, not a person's name — names go stale; rotations do not).
- What to capture *before* escalating: logs, the diagnosis output, what you
  already tried.

### 7. Root cause and follow-up

A short pointer, not the full analysis:

- The underlying cause, if known.
- Link to the postmortem or the tracking issue for the permanent fix.
- A note on when this runbook can be retired (i.e. what fix makes it obsolete).

## Quality rules

- **Commands must be copy-pasteable.** A command with an unexplained placeholder
  or a missing flag fails the 3am test.
- **State expected output.** "Run this" without "you should see X" leaves the
  operator unable to tell success from failure.
- **Lead with the action, follow with the why.** The operator needs to act
  first and understand later; put rationale in a trailing sentence, not before
  the command.
- **No prose where a command will do.** Runbooks are read under stress. Numbered
  steps and code blocks beat paragraphs.
- **Date it and own it.** A runbook with no last-verified date and no owner rots
  silently. Re-verify after the system it describes changes.

## Keep it alive

A runbook is only as good as its last verification. Re-run it (in staging, or as
a game day) periodically. When a step no longer matches reality, the runbook is
lying to the next operator — fix it the moment you notice, the same way you would
fix a broken alert.
