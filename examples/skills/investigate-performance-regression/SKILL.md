---
name: investigate-performance-regression
description: Narrow down what made something slower using measurement, not intuition.
metadata:
  type: engineering
  version: 1
---
# Investigating a performance regression

Performance work goes wrong when it is driven by intuition about what is slow.
Your intuition is frequently wrong about where time actually goes. The whole
method here is to replace guessing with measurement at every step: reproduce,
measure, compare, find the hot path, change one thing, measure again.

The cardinal rule: **measure before and after every change.** A "fix" you did
not measure is a guess you got attached to.

## 1. Reproduce it reliably

You cannot fix what you cannot trigger on demand.

- Pin down the conditions: which operation, what input size, how much load,
  warm or cold cache, which environment.
- Build a repeatable harness — a benchmark, a load test, or a script — that
  produces the slow behavior consistently. Run it several times; if the number
  swings wildly, stabilize the harness before trusting any measurement.
- Quantify the regression in one sentence: "p95 of `/search` went from 120ms to
  900ms after the 4.2 deploy." A vague "it feels slow" cannot be confirmed
  fixed.

## 2. Establish the baseline

Find a known-good point to compare against.

- A previous release, a previous commit, a different environment, or a
  competitor operation that is fast.
- Measure the baseline with the *same* harness you will use for the regressed
  case. Comparing two numbers gathered different ways measures your methodology,
  not the system.
- If the regression appeared between two known points, `git bisect` with the
  benchmark as the predicate finds the introducing commit directly. This is
  often the fastest route to the cause and skips the rest of this process.

## 3. Profile — find where the time goes

Do not read code looking for slow lines. Profile and let the data point.

- Use a profiler appropriate to the symptom: a CPU profiler for compute-bound
  slowness, an allocation profiler for GC/memory pressure, a tracer or query log
  for I/O- and network-bound slowness.
- Distinguish the bottleneck *class* first: is the time in CPU, in waiting on
  I/O, in lock contention, or in GC? The fix for each is completely different,
  and the profiler tells you which without guessing.
- Find the **hot path** — the small fraction of code where the large fraction of
  time goes. Profiles are almost always lopsided; one or two frames usually
  dominate.
- See `@./symptoms.md` for mapping common symptoms to the likely cause and the
  tool that confirms it.

## 4. Form one hypothesis

State a specific, falsifiable claim about the cause:

- "The N+1 query in `load_orders` runs once per row; with 500 rows that is 500
  round-trips."
- "We deserialize the whole 10MB config on every request instead of caching it."
- "A lock around the cache serializes all readers under load."

A hypothesis you cannot test is not yet a hypothesis. It should predict what a
measurement will show.

## 5. Validate with a controlled experiment

Change exactly one thing and measure.

- Make the smallest change that tests the hypothesis — even a hacky one is fine
  at this stage; you are testing the theory, not shipping the fix.
- Run the *same* harness. Did the metric move the way your hypothesis predicted?
- If yes, you have found a real cause. If no, the hypothesis was wrong — discard
  it and return to the profile. Do not keep the change "just in case"; an
  unvalidated change is noise that will confuse the next measurement.
- Change one variable at a time. Two simultaneous changes make the result
  uninterpretable.

## 6. Fix properly, then re-measure

- Replace the experimental hack with a clean fix that addresses the cause.
- Re-run the harness: confirm the metric is back to (or better than) baseline.
- Confirm you did not move the cost elsewhere — a CPU win that triples memory, or
  a latency win that drops throughput, may be a net loss. Check the neighbors.
- Add a regression guard: a benchmark in CI, an alert threshold, or at minimum a
  documented number, so the same regression is caught next time instead of
  rediscovered.

## Common traps

- **Optimizing the wrong thing.** The function that *looks* expensive is often
  not where the time goes. Trust the profile over the code reading.
- **Micro-optimizing outside the hot path.** Speeding up code that is 2% of the
  time can at best save 2%. Spend effort where the profile is fat.
- **Measuring a cold or noisy system.** First-run numbers include JIT warmup,
  cold caches, and connection setup. Warm up, then measure; take several samples
  and look at the distribution, not one run.
- **Benchmarking in an environment unlike production.** A laptop SSD, an empty
  database, or a single client can hide the very contention that causes the
  production regression.
- **Stopping at the first plausible cause.** Confirm it accounts for the *size*
  of the regression. If the N+1 explains 80ms but you lost 780ms, keep looking.
