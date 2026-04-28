# Offload A/B Report

Run: 0f29aca3-63d9-4ad3-aaf9-dc44667cd442
Date: 2026-04-28T14:31:52.192650643+00:00
Commit: 
Workload: 128 B / 128 B request-response, N=5000 per config, warmup=500

## Summary Table

| Config | Features | p50 (ns) | p99 (ns) | p999 (ns) | delta_p99 vs baseline | Decision |
|---|---|---|---|---|---|---|
| baseline | (none) | 35589.00 | 41800.00 | 46189.00 | — | — |
| tx-cksum-only | hw-offload-tx-cksum | 35810.00 | 43170.00 | 48590.00 | -1370.00 ns | NoSignal |
| rx-cksum-only | hw-offload-rx-cksum | 35420.00 | 42140.00 | 47480.00 | -340.00 ns | NoSignal |
| mbuf-fast-free-only | hw-offload-mbuf-fast-free | 36260.00 | 44140.00 | 49010.00 | -2340.00 ns | NoSignal |
| rss-hash-only | hw-offload-rss-hash | 37580.00 | 46170.00 | 55890.00 | -4370.00 ns | NoSignal |
| rx-timestamp-only | hw-offload-rx-timestamp | 37229.00 | 45290.00 | 54700.00 | -3490.00 ns | NoSignal |
| llq-verify-only | hw-verify-llq | 35989.00 | 44370.00 | 50750.00 | -2570.00 ns | NoSignal |
| full | hw-offloads-all | 35840.00 | 44500.00 | 51880.00 | -2700.00 ns | NoSignal |

Noise floor (2 back-to-back baselines, |p99 delta|): 371.00 ns (raw); 371.00 ns (clamped)
Decision threshold (3 × clamped noise floor): 1113.00 ns

## Sanity Invariant

full p99: 44500.00 ns
Best individual p99: 42140.00 ns (rx-cksum-only)
-> VIOLATION: sanity invariant violated: full p99 44500 > best individual p99 42140 (offloads did not compose; investigate contention / false-sharing)

## Commit History

(git log unavailable)

## Full CSV

See `/tmp/bench-offload-ab-out/0f29aca3-63d9-4ad3-aaf9-dc44667cd442.csv`.
