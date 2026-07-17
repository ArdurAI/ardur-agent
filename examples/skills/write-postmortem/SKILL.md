---
name: write-postmortem
description: Write a blameless postmortem that turns an incident into durable improvements.
metadata:
  type: incident
  version: 1
---
# Writing a postmortem

A postmortem exists to make the same failure less likely next time. It is not a
status report, not a defense, and not a record of who was at fault. Its value is
entirely in the action items it produces and whether they get done.

The single most important word is **blameless**. The moment a postmortem reads
as "Alice deployed the bad change", people stop being honest, and you lose the
information you need to actually fix the system. Humans operating a system as
designed will occasionally make mistakes; if a single human error could take
the system down, the *system* is the problem to fix, not the human.

Use the skeleton in `@./template.md` to draft. The guidance below is how to fill
it well.

## 1. Timeline — what happened, when

A factual, timestamped sequence. No interpretation yet.

- Start before the incident: the change or condition that set it up.
- Mark the key moments: when it began, when it was detected, when it was
  acknowledged, key diagnostic and mitigation steps, when it was resolved.
- Use absolute timestamps with a timezone. "About 20 minutes later" is useless
  to a future reader reconstructing the sequence.
- Write what was *known at the time*, not what you know now with hindsight. "The
  on-call saw queue depth rising and assumed a traffic spike" is honest and
  useful; "obviously it was the bad deploy" is hindsight and unfair.

Two derived numbers fall out of a good timeline and are worth stating:
**time-to-detect** (incident start to detection) and **time-to-resolve**
(detection to resolution). They tell you whether to invest in better alerting or
faster mitigation.

## 2. Impact — who and how much, quantified

- User-facing effect: what could users not do, and for how long.
- Scope: how many users, which regions/tenants, what fraction of traffic.
- Quantify it: requests failed, revenue affected, SLA/error budget consumed,
  data lost or delayed. A number makes the cost concrete and justifies the
  follow-up work.

## 3. Contributing factors — not "the root cause"

Real incidents rarely have a single root cause. They have a chain of
contributing factors that lined up. Resist the urge to name one culprit.

- Ask "why" repeatedly, but branch — most "whys" have more than one answer.
- Cover the layers: the trigger (what changed), the vulnerability (why the
  system was susceptible), why it was not caught earlier (testing, review,
  staging gap), why detection was slow (alerting gap), why mitigation was slow
  (missing runbook, unclear ownership, risky rollback).
- Keep it about systems and processes. "The deploy had no canary stage" is a
  fixable factor; "the engineer should have been more careful" is not.

## 4. What went well

Genuinely include this, and not as a formality.

- What limited the damage? Good alerting, a fast rollback, a feature flag that
  let you disable the path, a teammate who knew the system.
- These are your existing defenses. Naming them tells you what to protect and
  what to replicate elsewhere. A postmortem that only lists failures teaches the
  org to fear incidents instead of learning from them.

## 5. What we will change — action items with owners

This is the part that matters. Everything above is context for this.

- Each action item has a **single named owner** and a **due date**. An item
  owned by "the team" is owned by no one and will not happen.
- Make them specific and verifiable: "Add a canary stage to the deploy pipeline
  for service X" beats "improve deploy safety".
- Prioritize by leverage. Prefer changes that prevent the *class* of failure
  (make the bad state impossible, add a guardrail) over changes that address
  this one instance (fix this one config).
- Be honest about scope. Five action items that get done beat twenty that rot in
  a backlog. Cut the list to what the team will actually complete.
- Track them where the team tracks work, and review them. An action item that is
  never followed up is the postmortem failing at its one job.

## Tone and process

- Write in neutral, factual language. Replace names with roles ("the on-call
  engineer") unless crediting someone for good work.
- Circulate a draft and hold a review with the people involved — the discussion
  surfaces contributing factors the author missed.
- Publish it where the org can find and search it. A postmortem read only by its
  author teaches only its author.
- Close the loop: when the action items are done, the postmortem's purpose is
  fulfilled. Until then it is an open promise.

## Smells of a bad postmortem

- Names a person as the cause.
- Has one "root cause" and no contributing factors.
- Action items with no owner or no date.
- "Be more careful" / "add more testing" as an action item (not specific, not
  verifiable).
- No "what went well" section.
- Written, filed, and never acted on.
