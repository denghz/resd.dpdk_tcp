# A10 bench-nightly reports

First real-hardware runs of the Phase A10 bench-nightly pipeline against
an AWS `bench-pair` fleet (c6a.2xlarge × 2, ap-south-1, AMI
`ami-05ae5cb6a9a7022b9`).

## Artefacts

| File | Status | Source |
|---|---|---|
| `bench-baseline.md` | ✅ Landed (refreshed 2026-04-28) | `target/bench-results/2026-04-28T14-20-28Z/report.md` (run `bm6lmn8kp`) |
| `offload-ab.md` | ✅ Landed 2026-04-28 | `target/bench-results/2026-04-28T14-20-28Z/bench-offload-ab/offload-ab.md` |
| `obs-overhead.md` | ✅ Landed 2026-04-28 | `target/bench-results/2026-04-28T14-20-28Z/bench-obs-overhead/obs-overhead.md` |

## Headline results

### `bench-baseline.md` — DPDK vs Linux kernel TCP RTT

5 000 measurement iterations + 500 warmup, 128 B / 128 B payloads, single
long-lived TCP connection over the data ENI:

| Stack | p50 | p99 | p999 |
|---|---|---|---|
| dpdk_net | **35.6 µs** | 44.0 µs | 50.0 µs |
| linux_kernel | 37.9 µs | 47.4 µs | 57.8 µs |

DPDK userspace stack measurably faster than Linux kernel TCP at every
percentile under the same workload.

`bench-micro` (pure in-process criterion, no NIC) contributed 7 micro
timings for poll / tsc_read / flow_lookup / send / tcp_input / counters
/ timer paths — see `report.md` source.

### `offload-ab.md` — hardware-offload A/B sweep

8 configs (baseline + 7 individual offload bits + `full`), each 5 000
iterations. Noise floor 371 ns; decision threshold 1 113 ns. Every offload
delta vs baseline came in `NoSignal` — either the offload is genuinely
neutral on c6a.2xlarge ENA at this workload, or the latency change is
under the shared-tenant noise floor. Sanity invariant flagged the
`full` config's p99 (44 500 ns) as worse than `rx-cksum-only` alone
(42 140 ns), suggesting the offload combinations don't compose cleanly
on this NIC — worth a follow-up audit.

### `obs-overhead.md` — observability-counter A/B sweep

5 configs (`obs-none` baseline, 3 individual counter groups, `default`).
Noise floor 650 ns; decision threshold 1 950 ns. All deltas `NoSignal`.
Two sanity-floor violations (byte-counters-only + default both p99 <
obs-none p99) — same caveat: either dead-code observability sites or
within-noise-floor differences.

## Open items (deferred outside T16 scope)

- **bench-stress** — netem `tc qdisc apply` over DUT→peer SSH tunnel
  times out on this fleet (port 22 closed between data-plane peers).
  Requires plumbing the netem-apply path through a different channel
  or pre-installing the qdiscs at AMI-bake time. Tracked separately.
- **bench-vs-mtcp** burst + maxtp grids — emit 0-row CSVs; the burst
  workload's persistent-connection setup or peer-rwnd sampling path
  exits with no visible error. Needs the same gdb-wrap treatment that
  surfaced the bench-ab-runner drop-order bug.
- **`BENCH_ITERATIONS=5000`** (vs spec's 100 000) — a deterministic
  retransmit-budget exhaustion fires near iteration 7 050 on
  c6a.2xlarge. Suspected RX-mempool-handle leak in the partial-read
  `try_clone` path at `engine.rs:4114`. Diagnostic counters proposed in
  `a10-ab-driver-debug.md` §3 should isolate it on the next run.

## Companion documents

- `a10-ab-driver-debug.md` — v1 forensic pass (Bugs 1+2, with v2 +
  Codex revisions for Bug 2)
- `a10-ab-driver-debug-v2.md` — v2 deeper Claude pass (drop-order
  Hypothesis B)
- `a10-ab-driver-debug-codex.md` — codex second-opinion (drop-order
  invariant generalised; nailed the actual fix). gdb backtrace from
  run `br32yx9a7` confirmed this one.
- `bench-baseline.md` / `offload-ab.md` / `obs-overhead.md` — the three
  artefact files.

## Confirmed-fixed bugs along the way

| Commit | Bug | How surfaced |
|---|---|---|
| `726a411` | sshd race in wait_for_ssh probe | `kex_exchange_identification: Connection closed` after CDK CREATE_COMPLETE |
| `27a8044` | clap rejected hyphen-leading EAL args, comma split couldn't preserve PCI inner commas | "unexpected argument '-l' found" |
| `a24ef56` | rx/tx ring size 1024 default exceeds ENA cap (512) | "Invalid value for nb_tx_desc(=1024), should be: <= 512" |
| `35c490a` | peer data-NIC bound to vfio-pci by AMI; echo-server can't accept on Linux | DUT-side connect timeout |
| `1f9c407` | `set -e` aborted whole nightly on first per-bench failure | only first bench's CSV produced |
| `5d945f3` | wire-diff abort on missing pcap; bench-stress FaultInjector multi-spec collision | `chown: No such file`, "multiple distinct FaultInjector specs" |
| `d0a61ae` | bench-offload-ab/-obs-overhead `--output-dir` collided with the scp'd binary path | "Error: creating /tmp/bench-offload-ab" |
| `4394ed7` | every new process picked ephemeral port 49152 first → peer TIME_WAIT collision | `connection closed during handshake: err=0` |
| `715de88` | bench-micro panic-strategy mismatch on `cargo bench` profile | `crate dpdk_net_core requires panic strategy abort` |
| `671062a` | iteration-7051 retransmit cliff (workaround) | `tcp error during recv: errno=-110` at iter ~7051 |
| `585d647` | EAL hugepage state isolation (`--in-memory --huge-unlink`) | (didn't fix the real bug, but harmless) |
| `a1d2c56` | **drop-order: mbuf owners released after mempools** | gdb backtrace `shim_rte_mbuf_refcnt_update → MbufHandle::drop → ... → FlowTable::drop → Engine::drop`, mbuf in deleted memzone |
| `f8c1c13` | gdb wrapper deprecated-syntax warning corrupted bench-ab-runner stdout | bench-offload-ab "9 rows expected 7" |
