# resd.dpdk_tcp A10 Bench Report

**Run:** `3f247107-5c40-4dbe-a1e5-b3fbe057a60b`
**Commit:** ``
**Branch:** ``
**Date:** 2026-04-28T14:27:38.240845414+00:00
**Host:** ip-10-0-0-90 ()
**CPU:** AMD EPYC 7R13 Processor
**DPDK:** 23.11.0
**Kernel:** 6.8.0-1052-aws
**NIC:** 
**AMI:** 
**Precondition mode:** strict

## Preconditions

| Check | Status |
|---|---|
| isolcpus | `pass=2-7` |
| nohz_full | `pass=2-7` |
| rcu_nocbs | `pass=2-7` |
| governor | `pass=no-cpufreq-subsystem` |
| cstate_max | `pass=C1` |
| tsc_invariant | `pass` |
| coalesce_off | `pass=skipped` |
| tso_off | `pass=skipped` |
| lro_off | `pass=skipped` |
| rss_on | `pass=skipped` |
| thermal_throttle | `pass=0` |
| hugepages_reserved | `pass=2048` |
| irqbalance_off | `pass` |
| wc_active | `pass=deferred` |

## bench-e2e

| test_case | feature_set | dimensions | metric | unit | agg | value | mode |
|---|---|---|---|---|---|---|---|
| request_response_rtt | trading-latency | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p50 | 35680 | strict |
| request_response_rtt | trading-latency | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p99 | 42740 | strict |
| request_response_rtt | trading-latency | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p999 | 50680 | strict |
| request_response_rtt | trading-latency | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | mean | 36019.3332 | strict |
| request_response_rtt | trading-latency | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | stddev | 2304.1646 | strict |
| request_response_rtt | trading-latency | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_lower | 35955.4650 | strict |
| request_response_rtt | trading-latency | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_upper | 36083.2014 | strict |

## bench-obs-overhead

| test_case | feature_set | dimensions | metric | unit | agg | value | mode |
|---|---|---|---|---|---|---|---|
| request_response_rtt | obs-none | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p50 | 35829 | strict |
| request_response_rtt | obs-none | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p99 | 43240 | strict |
| request_response_rtt | obs-none | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p999 | 48240 | strict |
| request_response_rtt | obs-none | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | mean | 36145.8298 | strict |
| request_response_rtt | obs-none | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | stddev | 2302.6801 | strict |
| request_response_rtt | obs-none | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_lower | 36082.0028 | strict |
| request_response_rtt | obs-none | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_upper | 36209.6568 | strict |
| request_response_rtt | poll-saturation-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p50 | 35929 | strict |
| request_response_rtt | poll-saturation-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p99 | 43669 | strict |
| request_response_rtt | poll-saturation-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p999 | 49789 | strict |
| request_response_rtt | poll-saturation-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | mean | 36266.4058 | strict |
| request_response_rtt | poll-saturation-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | stddev | 2224.1497 | strict |
| request_response_rtt | poll-saturation-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_lower | 36204.7555 | strict |
| request_response_rtt | poll-saturation-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_upper | 36328.0561 | strict |
| request_response_rtt | byte-counters-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p50 | 35440 | strict |
| request_response_rtt | byte-counters-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p99 | 42290 | strict |
| request_response_rtt | byte-counters-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p999 | 50949 | strict |
| request_response_rtt | byte-counters-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | mean | 35767.6014 | strict |
| request_response_rtt | byte-counters-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | stddev | 2545.3496 | strict |
| request_response_rtt | byte-counters-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_lower | 35697.0479 | strict |
| request_response_rtt | byte-counters-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_upper | 35838.1549 | strict |
| request_response_rtt | obs-all-no-none | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p50 | 35830 | strict |
| request_response_rtt | obs-all-no-none | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p99 | 43700 | strict |
| request_response_rtt | obs-all-no-none | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p999 | 48849 | strict |
| request_response_rtt | obs-all-no-none | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | mean | 36155.9576 | strict |
| request_response_rtt | obs-all-no-none | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | stddev | 2220.0006 | strict |
| request_response_rtt | obs-all-no-none | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_lower | 36094.4223 | strict |
| request_response_rtt | obs-all-no-none | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_upper | 36217.4929 | strict |
| request_response_rtt | default | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p50 | 35620 | strict |
| request_response_rtt | default | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p99 | 43120 | strict |
| request_response_rtt | default | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p999 | 48800 | strict |
| request_response_rtt | default | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | mean | 35933.9240 | strict |
| request_response_rtt | default | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | stddev | 2132.7466 | strict |
| request_response_rtt | default | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_lower | 35874.8073 | strict |
| request_response_rtt | default | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_upper | 35993.0407 | strict |
| request_response_rtt | obs-none-noise-1 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p50 | 35889 | strict |
| request_response_rtt | obs-none-noise-1 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p99 | 44590 | strict |
| request_response_rtt | obs-none-noise-1 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p999 | 49029 | strict |
| request_response_rtt | obs-none-noise-1 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | mean | 36296.0178 | strict |
| request_response_rtt | obs-none-noise-1 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | stddev | 2552.2106 | strict |
| request_response_rtt | obs-none-noise-1 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_lower | 36225.2741 | strict |
| request_response_rtt | obs-none-noise-1 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_upper | 36366.7615 | strict |
| request_response_rtt | obs-none-noise-2 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p50 | 36170 | strict |
| request_response_rtt | obs-none-noise-2 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p99 | 45240 | strict |
| request_response_rtt | obs-none-noise-2 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p999 | 50320 | strict |
| request_response_rtt | obs-none-noise-2 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | mean | 36560.8380 | strict |
| request_response_rtt | obs-none-noise-2 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | stddev | 2301.0901 | strict |
| request_response_rtt | obs-none-noise-2 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_lower | 36497.0550 | strict |
| request_response_rtt | obs-none-noise-2 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_upper | 36624.6210 | strict |

## bench-offload-ab

| test_case | feature_set | dimensions | metric | unit | agg | value | mode |
|---|---|---|---|---|---|---|---|
| request_response_rtt | baseline | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p50 | 35589 | strict |
| request_response_rtt | baseline | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p99 | 41800 | strict |
| request_response_rtt | baseline | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p999 | 46189 | strict |
| request_response_rtt | baseline | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | mean | 35909.6584 | strict |
| request_response_rtt | baseline | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | stddev | 2300.5216 | strict |
| request_response_rtt | baseline | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_lower | 35845.8912 | strict |
| request_response_rtt | baseline | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_upper | 35973.4256 | strict |
| request_response_rtt | tx-cksum-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p50 | 35810 | strict |
| request_response_rtt | tx-cksum-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p99 | 43170 | strict |
| request_response_rtt | tx-cksum-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p999 | 48590 | strict |
| request_response_rtt | tx-cksum-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | mean | 36168.4378 | strict |
| request_response_rtt | tx-cksum-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | stddev | 2307.7961 | strict |
| request_response_rtt | tx-cksum-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_lower | 36104.4690 | strict |
| request_response_rtt | tx-cksum-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_upper | 36232.4066 | strict |
| request_response_rtt | rx-cksum-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p50 | 35420 | strict |
| request_response_rtt | rx-cksum-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p99 | 42140 | strict |
| request_response_rtt | rx-cksum-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p999 | 47480 | strict |
| request_response_rtt | rx-cksum-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | mean | 35764.6798 | strict |
| request_response_rtt | rx-cksum-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | stddev | 2314.2968 | strict |
| request_response_rtt | rx-cksum-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_lower | 35700.5308 | strict |
| request_response_rtt | rx-cksum-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_upper | 35828.8288 | strict |
| request_response_rtt | mbuf-fast-free-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p50 | 36260 | strict |
| request_response_rtt | mbuf-fast-free-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p99 | 44140 | strict |
| request_response_rtt | mbuf-fast-free-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p999 | 49010 | strict |
| request_response_rtt | mbuf-fast-free-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | mean | 36598.8914 | strict |
| request_response_rtt | mbuf-fast-free-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | stddev | 2827.5044 | strict |
| request_response_rtt | mbuf-fast-free-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_lower | 36520.5170 | strict |
| request_response_rtt | mbuf-fast-free-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_upper | 36677.2658 | strict |
| request_response_rtt | rss-hash-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p50 | 37580 | strict |
| request_response_rtt | rss-hash-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p99 | 46170 | strict |
| request_response_rtt | rss-hash-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p999 | 55890 | strict |
| request_response_rtt | rss-hash-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | mean | 38016.4224 | strict |
| request_response_rtt | rss-hash-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | stddev | 2865.4645 | strict |
| request_response_rtt | rss-hash-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_lower | 37936.9958 | strict |
| request_response_rtt | rss-hash-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_upper | 38095.8490 | strict |
| request_response_rtt | rx-timestamp-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p50 | 37229 | strict |
| request_response_rtt | rx-timestamp-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p99 | 45290 | strict |
| request_response_rtt | rx-timestamp-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p999 | 54700 | strict |
| request_response_rtt | rx-timestamp-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | mean | 37618.6218 | strict |
| request_response_rtt | rx-timestamp-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | stddev | 3019.0727 | strict |
| request_response_rtt | rx-timestamp-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_lower | 37534.9374 | strict |
| request_response_rtt | rx-timestamp-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_upper | 37702.3062 | strict |
| request_response_rtt | llq-verify-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p50 | 35989 | strict |
| request_response_rtt | llq-verify-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p99 | 44370 | strict |
| request_response_rtt | llq-verify-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p999 | 50750 | strict |
| request_response_rtt | llq-verify-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | mean | 36330.8810 | strict |
| request_response_rtt | llq-verify-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | stddev | 2538.0296 | strict |
| request_response_rtt | llq-verify-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_lower | 36260.5304 | strict |
| request_response_rtt | llq-verify-only | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_upper | 36401.2316 | strict |
| request_response_rtt | full | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p50 | 35840 | strict |
| request_response_rtt | full | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p99 | 44500 | strict |
| request_response_rtt | full | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p999 | 51880 | strict |
| request_response_rtt | full | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | mean | 36199.6454 | strict |
| request_response_rtt | full | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | stddev | 2373.0654 | strict |
| request_response_rtt | full | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_lower | 36133.8674 | strict |
| request_response_rtt | full | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_upper | 36265.4234 | strict |
| request_response_rtt | baseline-noise-1 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p50 | 35500 | strict |
| request_response_rtt | baseline-noise-1 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p99 | 43329 | strict |
| request_response_rtt | baseline-noise-1 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p999 | 47760 | strict |
| request_response_rtt | baseline-noise-1 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | mean | 35870.3786 | strict |
| request_response_rtt | baseline-noise-1 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | stddev | 2121.8861 | strict |
| request_response_rtt | baseline-noise-1 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_lower | 35811.5629 | strict |
| request_response_rtt | baseline-noise-1 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_upper | 35929.1943 | strict |
| request_response_rtt | baseline-noise-2 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p50 | 36069 | strict |
| request_response_rtt | baseline-noise-2 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p99 | 43700 | strict |
| request_response_rtt | baseline-noise-2 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p999 | 50329 | strict |
| request_response_rtt | baseline-noise-2 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | mean | 36422.4420 | strict |
| request_response_rtt | baseline-noise-2 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | stddev | 2424.0062 | strict |
| request_response_rtt | baseline-noise-2 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_lower | 36355.2520 | strict |
| request_response_rtt | baseline-noise-2 | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_upper | 36489.6320 | strict |

## bench-vs-linux

| test_case | feature_set | dimensions | metric | unit | agg | value | mode |
|---|---|---|---|---|---|---|---|
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"dpdk_net"}` | rtt_ns | ns | p50 | 36140 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"dpdk_net"}` | rtt_ns | ns | p99 | 44520 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"dpdk_net"}` | rtt_ns | ns | p999 | 49240 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"dpdk_net"}` | rtt_ns | ns | mean | 36529.4486 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"dpdk_net"}` | rtt_ns | ns | stddev | 3214.8548 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"dpdk_net"}` | rtt_ns | ns | ci95_lower | 36440.3374 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"dpdk_net"}` | rtt_ns | ns | ci95_upper | 36618.5598 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"linux_kernel"}` | rtt_ns | ns | p50 | 37851 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"linux_kernel"}` | rtt_ns | ns | p99 | 47401 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"linux_kernel"}` | rtt_ns | ns | p999 | 59121 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"linux_kernel"}` | rtt_ns | ns | mean | 38285.3312 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"linux_kernel"}` | rtt_ns | ns | stddev | 2463.5341 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"linux_kernel"}` | rtt_ns | ns | ci95_lower | 38217.0455 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"linux_kernel"}` | rtt_ns | ns | ci95_upper | 38353.6169 | strict |

