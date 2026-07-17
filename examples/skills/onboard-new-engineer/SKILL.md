---
name: onboard-new-engineer
description: Structure a new engineer's first week so they ship and build context, not just read.
metadata:
  type: onboarding
  version: 1
---
# Onboarding a new engineer

The goal of the first week is not to teach someone everything about the system —
that takes months. It is to get them to their **first merged change** quickly,
and to build the map and relationships they will fill in over time. An engineer
who has shipped one small thing and knows who to ask is far more productive than
one who has read documentation for five days.

This is a generic structure. It is meant to be customized per team — see
`@./customize.md` for the specific things you must fill in before handing it to a
new hire. A runbook with `<placeholder>` repo names is worse than none.

## Before day one (the host's job)

The new engineer's time is wasted if the basics are not ready. Have these done
*before* they start:

- Accounts and access provisioned: code host, CI, cloud console, observability,
  chat, ticketing, password manager.
- A named **onboarding buddy** assigned — one person whose explicit job is to
  answer "dumb" questions without the new hire feeling they are imposing.
- A **starter task** picked: a small, real, low-risk change with a clear
  definition of done. Not a toy; something that actually ships.

## Day 1-2: orient and set up

The aim is a working environment and a mental map, not mastery.

1. **Read the right things, in order.** Point at the few documents that give the
   most context per minute: the architecture overview, the "how we work" doc
   (branching, review, deploy), and the top-level README. Do not hand over a
   wiki and say "read everything" — curate three to five documents.
2. **Set up the dev environment end to end.** Clone, build, run the app locally,
   run the test suite green. This is the highest-value early task: it forces
   contact with the real toolchain and surfaces setup gaps. Every place the
   setup docs are wrong, the new hire fixes them — their first contribution.
3. **Run the app and poke it.** Use the product as a user. Trigger the main
   flows. Nothing builds intuition faster than seeing the thing work.
4. **Meet the buddy** and agree on a check-in rhythm (a short daily sync for the
   first week works well).

## Day 2-4: ship a small change

The centerpiece of the week.

- Pick up the prepared starter task. Keep it genuinely small — a bug fix, a
  small feature, a doc or test gap.
- Let them do it, with the buddy on call. Resist doing it for them; struggling
  through the first change is how the workflow gets learned.
- Walk the **full path to production**: branch, change, test, open a PR, get it
  reviewed, address feedback, merge, watch it deploy. The point is the *path*,
  not the change. After this, every future change is a variation on a known
  process.
- Treat the first PR's review as teaching, not gatekeeping. Explain the *why*
  behind comments.

## Day 4-5: widen the map

Now that they have shipped once, broaden context.

- **Shadow on-call** or sit in on an incident/triage if one occurs. Seeing how
  the team responds to problems teaches the system's failure modes and the human
  side of operating it.
- **Meet the people**, with purpose. Short 1:1s with the tech lead, the
  product/eng manager, and one or two engineers on adjacent systems. Each
  conversation should answer "what do you own and when would I come to you?"
- **Read one thing deeply.** Pick the subsystem they will work in next and have
  them trace one request or one flow through it end to end, asking the buddy
  where they get stuck.

## End of week 1: check in

A short retrospective with the buddy and/or manager:

- What is still unclear? (These gaps are next week's reading list.)
- What in the setup or docs was wrong or missing? (Fix it now, while it is
  fresh — they are the last person to see it with fresh eyes.)
- What will they work on next?

## Principles

- **Ship early.** A merged change in week one beats a week of reading. It proves
  the toolchain works for them and builds confidence.
- **Curate, do not dump.** Three good documents beat a wiki. A linked maze of
  docs signals "we have not thought about your time".
- **Make asking easy.** A named buddy removes the social cost of questions. The
  fastest onboarding happens where no question feels dumb.
- **Onboarding docs are the new hire's first bug report.** Every rough edge they
  hit is one the *next* hire will hit too. Fixing the docs is real work, and
  they are uniquely positioned to do it.
- **Context over completeness.** They do not need to understand everything. They
  need to know enough to be useful and to know who to ask for the rest.
