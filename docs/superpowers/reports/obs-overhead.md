# Observability Overhead Report

Run: ff55b46d-47ac-4f4c-9361-11843a9517ff
Date: 2026-04-28T14:32:25.968382505+00:00
Commit: 
Workload: 128 B / 128 B request-response, N=5000 per config, warmup=500

## Summary Table

| Config | Features | p50 (ns) | p99 (ns) | p999 (ns) | delta_p99 vs obs-none | Decision | Default | Action (if fail) |
|---|---|---|---|---|---|---|---|---|
| obs-none | obs-none | 35829.00 | 43240.00 | 48240.00 | — | — | OFF | — |
| poll-saturation-only | obs-poll-saturation | 35929.00 | 43669.00 | 49789.00 | 429.00 ns | NoSignal | ON | — |
| byte-counters-only | obs-byte-counters | 35440.00 | 42290.00 | 50949.00 | -950.00 ns | NoSignal | OFF | — |
| obs-all-no-none | obs-all | 35830.00 | 43700.00 | 48849.00 | 460.00 ns | NoSignal | (composite) | — |
| default | (prod default) | 35620.00 | 43120.00 | 48800.00 | -120.00 ns | NoSignal | N/A | — |

Noise floor (2 back-to-back obs-none runs, |p99 delta|): 650.00 ns (raw); 650.00 ns (clamped)
Decision threshold (3 × clamped noise floor): 1950.00 ns

## Sanity Invariant

Lowest p99 (obs-none): 43240.00 ns
Any p99 < 43240.00 ns? YES -> VIOLATION (an observable is either dead code, a regression, or inside the noise floor)
- byte-counters-only: p99 = 42290.00 ns
- default: p99 = 43120.00 ns

Diagnostic: observability floor violated: config 'byte-counters-only' p99 42290 < obs-none p99 43240 (observability can only add cost; either the observable is dead code, the implementation regressed, or the delta is within measurement noise); observability floor violated: config 'default' p99 43120 < obs-none p99 43240 (observability can only add cost; either the observable is dead code, the implementation regressed, or the delta is within measurement noise)

## Decision → Action Recommendations

For each Signal with default=ON, the implementer reviews the table and picks one of the action-taxonomy options in a follow-up commit, NOT automated by the harness:

- **batch** — accumulate the increment in a per-poll local and `fetch_add` once per `poll_once`
- **remove** — eliminate the counter entirely
- **flip default** — move the feature from default-ON to default-OFF (opt-in)
- **move off hot path** — relocate the emission to a slow-path decision point

No Signal + default=ON rows this run — no action required.

## Commit History

(git log unavailable)

## Full CSV

See `/tmp/bench-obs-overhead-out/ff55b46d-47ac-4f4c-9361-11843a9517ff.csv`.
