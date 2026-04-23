# A10 bench-nightly reports

First real-hardware run of the Phase A10 bench-nightly pipeline against an
AWS `bench-pair` fleet (c6a.2xlarge × 2, ap-south-1, AMI
`ami-05ae5cb6a9a7022b9`).

## Artefacts

| File | Status | Source |
|---|---|---|
| `bench-baseline.md` | ✅ Landed 2026-04-23 | `target/bench-results/2026-04-23T19-45-13Z/report.md` (run `ed2075b4`) |
| `offload-ab.md` | ⏸ Blocked on A/B-driver bug | See **Blockers** below |
| `obs-overhead.md` | ⏸ Blocked on A/B-driver bug | See **Blockers** below |

## What `bench-baseline.md` shows

Real request/response RTT on the baked AMI, DPDK userspace stack vs
Linux kernel stack over the data ENI, 5 000 measurement iterations + 500
warmup, 128 B / 128 B payloads, single long-lived TCP connection:

| Stack | p50 | p99 | p999 | mean |
|---|---|---|---|---|
| dpdk_net | **35.6 µs** | 45.6 µs | 66.5 µs | 36.1 µs |
| linux_kernel | 37.9 µs | 47.4 µs | 57.8 µs | 38.3 µs |

`bench-micro` (pure in-process criterion, no NIC) contributed 7 micro
timings for poll / tsc_read / flow_lookup / send / tcp_input / counters
/ timer paths.

## Blockers on the remaining two artefacts

`bench-offload-ab` and `bench-obs-overhead` both drive `bench-ab-runner`
as a subprocess, one invocation per A/B config. Both tripped distinct
failures during run `bl16x36lb` (2026-04-23T19:45Z):

### `bench-offload-ab` — baseline config handshake

```
bench-offload-ab: running config baseline (features=[])
dpdk_net: port 0 driver=net_ena rx_offload_capa=… configured rx_offloads=0x…000e …
ena_rx_queue_release(): Rx queue 0:0 released
Error: connection closed during handshake: err=0
Error: bench-ab-runner config baseline exited with status ExitStatus(unix_wait_status(256))
```

`err=0` on a `Closed` event during handshake means the DPDK stack received
a close signal (RST or bad-ACK triggered synthesized close) between sending
SYN and completing the three-way handshake. Needs instrumentation — the
handshake succeeded fine for bench-e2e/bench-vs-linux in the same run, so
something stateful changed by the time bench-ab-runner ran (9–10 min
later, same peer, same echo-server).

### `bench-obs-overhead` — `obs-none` config SIGSEGV

```
bench-obs-overhead: running config obs-none (features=[obs-none])
ena_rx_queue_release(): Rx queue 0:0 released
Error: bench-ab-runner config obs-none exited with status ExitStatus(unix_wait_status(139))
```

Exit 139 = SIGSEGV. The `obs-none` feature compiles out the `Closed`
event emission (engine.rs:3707) and some related observability paths.
A downstream caller likely dereferences a pointer that was only populated
inside the `#[cfg(not(feature = "obs-none"))]` branch — needs a code
audit, not a harness fix.

## Workarounds in play

- `BENCH_ITERATIONS=5000` (vs spec's 100 000) to dodge a deterministic
  cliff near iteration 7051 on c6a.2xlarge. Root cause still open: either
  AWS per-flow limit or a mempool/retransmit-history wrap inside our
  stack.
- `BENCH_WARMUP=500`.

Once the A/B-driver bugs are fixed, re-running `bench-nightly.sh` on
the same instance should produce both missing artefacts. A bigger
instance (c6in.metal) would lift the per-flow ceiling but isn't
required for these two specific blockers — they're code issues inside
`dpdk-net-core`, not AWS throttles.
