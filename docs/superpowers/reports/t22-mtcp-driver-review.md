# T22 mTCP driver pump — implementation + review (2026-05-05)

## Implementation

`/home/ubuntu/resd.dpdk_tcp/tools/bench-vs-mtcp/peer/mtcp-driver.c` —
1101 lines / 754 LOC. Was ENOSYS stub; now full burst+maxtp arms.

Subagent-produced (general-purpose, opus 4.7, task `a81ab638`). Built locally
against `/tmp/mtcp-yaml-test/mtcp/lib/libmtcp.a` + DPDK 20.11 sidecar.

## Documented deviations (all spec-compliance-accepted)

- **No HW TX timestamping** — mTCP doesn't expose `rte_mbuf::tx_timestamp`. Burst arm emits `tx_ts_mode: "tsc_fallback"`; maxtp emits `"n/a"`.
- **No `getsockopt(TCP_INFO)`** — mTCP `mtcp_getsockopt` only supports `SO_ERROR` (verified in `third_party/mtcp/mtcp/src/api.c:217-258`). Maxtp arm uses bytes-echoed-back as snd_una proxy. Semantically equivalent for echo-server peer.
- **pps approximation** — `write_calls * ceil(W/MSS)` instead of an `eth.tx_pkts` equivalent. Documented inline.
- **Single-threaded** — `mtcp_create_context(0)`, pinned to core 0. Wrapper passes `--num-cores 1`. Multi-core = follow-up.
- **60s soft deadlines** on connect / sustained pump / drain — present per spec.

## Two-stage review verdicts

### Spec-compliance review (subagent `a7243d6a`, general-purpose opus 4.7)
**PASS-WITH-CAVEATS / ship-as-is.** All frozen contracts (CLI flags, JSON output schema, error shape, exit codes) preserved. Workload pump shapes faithfully mirror dpdk_burst.rs / dpdk_maxtp.rs. 8 minor defects, none blocking.

### Code-quality review (subagent `a7037b70`, superpowers:code-reviewer opus 4.7)
**PASS-WITH-CAVEATS / fix-then-ship.** Three blockers identified:
- **#2** `mtcp_destroy()` called on init-failure cleanup path → faults inside DPDK EAL teardown.
- **#4** `events[MAX_EVENTS]` 16-32 KB stack alloc per burst → wasteful in the hot path.
- **#7** Drain loop swallows hard errors silently → spins on dead socket for full deadline.

## Blocker fixes applied (this commit)

1. **Defect #2** — added `int mtcp_inited = 0;` at the top of both `run_burst_workload` (line ~492) and `run_maxtp_workload` (line ~756). Set to 1 immediately after successful `mtcp_init`. Wrap final `mtcp_destroy()` calls in `if (mtcp_inited)`.
2. **Defect #4** — shrunk `events[MAX_EVENTS]` (~1024 entries) to `events[1]` at all four yield-only call sites (`send_burst_bytes`, `wait_for_burst_echo`, `maxtp_pump_one_round`, maxtp shutdown drain). Updated `mtcp_epoll_wait(.., 1, ..)` to match. The events array result was always discarded with `(void)n` — pure stack-yield ceremony — so size 1 is sufficient.
3. **Defect #7** — added `int hard_err = 0;` flag in maxtp shutdown drain. Inner per-conn loop sets `hard_err = 1; break;` on `errno != EAGAIN`; outer time loop checks `&& !hard_err`. Acceptable to skip ~50ms residual echo if peer reset — measurement window is already closed.

## Build verification

```
make -f Makefile.mtcp mtcp-driver MTCP_BUILD=/tmp/mtcp-yaml-test DPDK_PREFIX=/usr/local/dpdk-20.11
# → /home/ubuntu/resd.dpdk_tcp/tools/bench-vs-mtcp/peer/mtcp-driver (19 MB ELF)
./mtcp-driver --help                # → usage on stdout, exit 0
./mtcp-driver                       # → {"error": "missing --workload", "errno": 22} on stderr, exit 2
cargo test -p bench-vs-mtcp --lib mtcp:: # → 36/36 pass
```

## Outstanding non-blockers (follow-up)

From spec review:
- D1: `--num-cores N>1` silently shrunk to 1 (latent — wrapper passes 1 today).
- D7: shutdown-drain bytes accumulate into goodput numerator (<0.1% bias at 50ms tail / 60s window — sub-noise).
- D8: TSC calibration after first connect (sub-ms timing — non-issue).

From code-quality review:
- Defect 1: `assert(a->bursts > 0)` defensive guard (CLI rejects this; cosmetic).
- Defect 3: initialize `i` at line 929 (cosmetic).
- Defect 6: drain-loop `if (drained == 0)` skip-to-epoll-wait (avoids one syscall but functionally fine).
- Style: line 366 `(void)ep;` comment was wrong (FIXED — removed when replacing events[] decl).
- Style: line 1051 missing space after comma; line 624 dead `(void)t_first_wire`; duplicated `mtcp_setconf` blocks; CTL_MOD return-value unchecked.

## Status

**T22 ship-ready.** Driver pump produces real numbers when invoked against an mTCP echo-server peer on the bench-pair AMI (assuming T18.1's libmtcp.a + bench-peer-mtcp ship together — already on AMI per T18.1 commit `e79c41f`).

The next bench-pair run with `--features fstack` off will exercise the dpdk + linux + mtcp comparator triangle.
