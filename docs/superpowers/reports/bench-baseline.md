# resd.dpdk_tcp A10 Bench Report

**Run:** `ed2075b4-0ecf-43d5-9aac-dd2fa65c7e14`
**Commit:** ``
**Branch:** ``
**Date:** 2026-04-23T19:52:11.696555522+00:00
**Host:** ip-10-0-0-58 ()
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
| request_response_rtt | trading-latency | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p50 | 35840 | strict |
| request_response_rtt | trading-latency | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p99 | 44049 | strict |
| request_response_rtt | trading-latency | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | p999 | 49220 | strict |
| request_response_rtt | trading-latency | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | mean | 36191.9660 | strict |
| request_response_rtt | trading-latency | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | stddev | 2200.0677 | strict |
| request_response_rtt | trading-latency | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_lower | 36130.9832 | strict |
| request_response_rtt | trading-latency | `{"request_bytes":128,"response_bytes":128}` | rtt_ns | ns | ci95_upper | 36252.9488 | strict |

## bench-vs-linux

| test_case | feature_set | dimensions | metric | unit | agg | value | mode |
|---|---|---|---|---|---|---|---|
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"dpdk_net"}` | rtt_ns | ns | p50 | 35640 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"dpdk_net"}` | rtt_ns | ns | p99 | 45620 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"dpdk_net"}` | rtt_ns | ns | p999 | 66540 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"dpdk_net"}` | rtt_ns | ns | mean | 36148.8048 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"dpdk_net"}` | rtt_ns | ns | stddev | 2893.4329 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"dpdk_net"}` | rtt_ns | ns | ci95_lower | 36068.6029 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"dpdk_net"}` | rtt_ns | ns | ci95_upper | 36229.0067 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"linux_kernel"}` | rtt_ns | ns | p50 | 37941 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"linux_kernel"}` | rtt_ns | ns | p99 | 47431 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"linux_kernel"}` | rtt_ns | ns | p999 | 57781 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"linux_kernel"}` | rtt_ns | ns | mean | 38279.9804 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"linux_kernel"}` | rtt_ns | ns | stddev | 2324.2797 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"linux_kernel"}` | rtt_ns | ns | ci95_lower | 38215.5547 | strict |
| rtt_comparison | trading-latency | `{"mode":"rtt","preset":"latency","stack":"linux_kernel"}` | rtt_ns | ns | ci95_upper | 38344.4061 | strict |

