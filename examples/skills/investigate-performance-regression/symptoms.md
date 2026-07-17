# Symptom-to-cause map

Use the symptom to pick where to look first and which tool confirms it. This
narrows the search; it does not replace measurement.

## High CPU, work is compute-bound
- Likely: an algorithm that got worse (O(n) -> O(n^2)), redundant work in a
  loop, serialization/parsing on a hot path, excessive logging.
- Confirm with: a CPU/sampling profiler. Look for the frame with the largest
  self-time.

## Latency high but CPU low — waiting, not working
- Likely: I/O. Slow queries, N+1 query patterns, synchronous calls to a slow
  dependency, DNS or connection setup per request, no connection pooling.
- Confirm with: a request tracer, the database slow-query log, or
  request-timing breakdowns. Time spent off-CPU is the tell.

## Throughput collapses under concurrency
- Likely: lock contention, a serialized critical section, a connection-pool or
  thread-pool exhausted, a shared resource bottleneck.
- Confirm with: a lock/contention profiler, pool saturation metrics, or
  throughput-vs-concurrency curves that flatten or invert.

## Memory growth, GC pauses, or OOM
- Likely: allocation on the hot path, a cache without eviction, accumulating
  buffers, large object retention, a leak.
- Confirm with: an allocation/heap profiler. Look at allocation rate and what
  retains memory across requests.

## Slow only at scale, fine in tests
- Likely: an unbounded operation that is cheap on small data — a full scan,
  loading an entire table, an unpaginated list, quadratic work that hides at
  n=10 and dominates at n=10000.
- Confirm with: running the harness at production-like data sizes, not test
  fixtures.

## Intermittent slow spikes
- Likely: GC pauses, cache stampede on expiry, a periodic background job
  contending for resources, a tail dependency timing out and retrying.
- Confirm with: time-correlated metrics — overlay the latency spikes with GC
  events, cache-miss rate, and background-job schedules to find what coincides.
