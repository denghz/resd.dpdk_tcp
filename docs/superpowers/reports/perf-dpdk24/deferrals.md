# DPDK 24.11 — pre-declared deferrals (not adopted in A10-perf)

These were marked deferred at brainstorm time (D6) and confirmed deferred during execution. Distinct from the T4.7-4.10 reports (`adopt-rte_lcore_var.md`, `adopt-rte_ptr_compress.md`, `adopt-rte_bit_atomic.md`, `adopt-ena-tx-logger.md`) which document outcomes of investigating APIs we considered actively.

## Per-CPU PM QoS resume latency (24.11)

**Status:** N/A for this workload. **Not investigated.**

The Stage 1 design is run-to-completion busy-poll per lcore (per `project_context` + spec §2). There is no wake-up path to optimize. `rte_power` / adaptive interrupt mode are unused. Per-CPU PM QoS resume latency would only matter if we ever moved to interrupt-driven RX or `rte_power`-aware sleep — both are explicit non-goals through Stage 3.

**Future revisit:** if Stage 4 (WAN hardening) or Stage 5+ ever explore power-aware operation modes for low-rate workloads, re-evaluate then. Until then, this remains correctly out-of-scope.

## `rte_thash_gen_key` (24.11)

**Status:** Deferred to Stage 2. **Not investigated.**

Stage 1 deployment is single-queue per port (per `feedback_subagent_model` + the A-HW phase's `RSS_HASH` plumbing-only design). There is no RSS imbalance to cure when there's exactly one queue. Manual or auto-generated keys are equivalent at queue count = 1.

When Stage 2 introduces multi-queue RSS (e.g. for higher-throughput market-data ingest scenarios), `rte_thash_gen_key` becomes a candidate for replacing whatever hardcoded RSS key we end up using for the initial multi-queue rollout. File for that phase, not this one.

**Future revisit:** Stage 2 multi-queue work.

## Intel E830 / `ice` driver (24.07)

**Status:** N/A — wrong NIC family. **Not investigated.**

Production target for Stage 1 deployment is AWS ENA (per `project_context` + the `bench-pair` AMI baked in `resd.aws-infra-setup`). `ice` is for Intel 800-series (E810/E830) — different vendor entirely. No reason to pull in a driver we don't use.

**Future revisit:** only if a future deployment surface introduces Intel SmartNIC / E830-class hardware. No such plan exists currently.

## Event pre-scheduling / `eventdev` `preschedule_type` (24.11)

**Status:** N/A — no eventdev path in our engine. **Not investigated.**

The Stage 1 engine is run-to-completion polling, not event-driven dispatch. We don't use `rte_event_*`, `rte_event_dev_*`, or any of the eventdev abstractions. Pre-scheduling events to ports before dequeue would require us to first build an eventdev-based pipeline — a large architectural change with no current motivation.

**Future revisit:** if Stage 5+ ever moves to a multi-stage pipeline (e.g. RX core → decode core → app core) for very-high-throughput market-data shapes. Highly unlikely for trading workload (≤100 long-lived connections).

## Cross-link to T4.7-4.10 outcomes

The 4 target APIs we DID investigate:

| API | Outcome | Report |
|---|---|---|
| `rte_lcore_var` | N/A — 0 candidate sites in our crate (architectural mismatch: single-Engine-per-lcore, not per-lcore-arrays) | `adopt-rte_lcore_var.md` |
| `rte_ptr_compress` | Deferred-to-e2e — 1 site exists in engine.rs RX burst but bench-micro doesn't reach it (no port in EngineNoEalHarness; send is stubbed) | `adopt-rte_ptr_compress.md` |
| `rte_bit_atomic_*` | N/A — 0 candidate sites; the codebase only uses `fetch_add` / `load` on counters, no `fetch_or` / `fetch_and` / `fetch_xor` patterns | `adopt-rte_bit_atomic.md` |
| ENA TX logger rework | Deferred-pending-T3.3 + bench-pair host — passive driver-side change requires real send measurement | `adopt-ena-tx-logger.md` |

Combined verdict: DPDK 24.11's API-surface improvements deliver **little measurable value at the bench-micro scope on a KVM dev host**. The improvements that COULD matter for our workload (`rte_ptr_compress` on real RX bursts, ENA TX logger on real send) require infrastructure (real ENA NIC, real send-path benches) that's deferred to T3.3 + bench-pair execution.

This is an **honest-deferral outcome**, not a missed optimization opportunity. Future re-evaluation: when T3.3 lands real send wiring AND a bench-pair host is provisioned, re-run T4.7-4.10's investigations with end-to-end measurement.
