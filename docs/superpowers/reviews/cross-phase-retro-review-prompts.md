# Cross-Phase Retrospective Review — Prompts + Workflow

**Audience:** an operator running a fresh Claude Code session (likely on a new
worktree off master) to drive a retrospective review of Stage 1 work to date
(phases A1 → A10.5).

**Purpose:** these are NOT the per-phase mTCP / RFC compliance reviews from
spec §10.13 / §10.14 — those already happened during each phase. This is a
ZOOMED-OUT review that catches what no individual phase review could have
caught: cross-phase architectural drift, accumulated tech debt, test-pyramid
gaps, observability completeness, hidden coupling between modules added at
different times.

**Strategy:** for each part, dispatch a Claude reviewer (architectural focus)
and a Codex reviewer (mechanical-defect focus) independently, then a small
synthesis subagent merges the two findings lists.

---

## Chunking strategy

Group A1 → A10.5 into nine architectural-layer parts. Each part is a coherent
slice an operator can hold in head when comparing the two reviews.

| Part | Phases | Theme |
|------|--------|-------|
| 1 | A1, A2 | Crate skeleton, EAL bring-up, L2/L3 (PMD wrapper, ARP, ICMP) |
| 2 | A3, A4 | TCP FSM core, options encode/decode, PAWS, reassembly, SACK |
| 3 | A5, A5.5, A5.6 | RACK / RTO / TLP / retransmit, event log forensics, RTT histogram |
| 4 | A6, A6.5, A6.6, A6.7 | Public C ABI, hot-path alloc elimination, RX zero-copy, FFI safety audit |
| 5 | A-HW, A-HW+ | ENA HW offloads + offload knobs |
| 6 | A7, A8, A8.5 | Packetdrill shim + loopback test server, tcpreq, observability gate, test coverage |
| 7 | A9 | Property/fuzz, libfuzzer targets, FaultInjector middleware |
| 8 | A10 | AWS bench-pair infra, benchmark harness, microbench, DPDK 23.11→24.x adopt |
| 9 | A10.5 | Layer H correctness gate (netem matrix) |

Per-part time budget: ~5–15 min per reviewer (Claude 3–8 min, codex 5–15 min)
plus 1–3 min synthesis. All 9 parts ≈ 1–2 hours of subagent time, gated on the
operator reading each synthesis before moving to the next part.

---

## Reusable Claude reviewer prompt

Dispatch via the `superpowers:code-reviewer` subagent (model: `opus`).

```
You are doing a cross-phase retrospective review of Stage 1 work in a Rust+DPDK
userspace TCP stack. Per-phase compliance reviews already happened (mTCP +
RFC gates per spec §10.13/§10.14, captured in docs/superpowers/reviews/
phase-aN-{mtcp-compare,rfc-compliance}.md). Your job is the OPPOSITE: zoom
out and find what no individual phase review could have caught.

## Part under review

<paste the part brief from below>

## Spec / plan / review docs to read first

- the phase's spec(s) and plan(s) under docs/superpowers/specs/ +
  docs/superpowers/plans/
- the phase's mTCP + RFC review reports under docs/superpowers/reviews/
- docs/superpowers/specs/2026-04-17-dpdk-tcp-design.md (parent spec) — only
  the §refs the part touches; do not re-read end-to-end.
- docs/superpowers/plans/stage1-phase-roadmap.md — for the phase's
  "Does NOT include" boundary.

## Phase-scoped diffs

`git log --oneline <prev-phase-tag>..<this-part-final-tag>` for the commit
range. Use `git show <SHA>` and `git diff <range> -- <path>` to inspect
the actual code changes, not just summaries.

## Review focus (this is the whole point of the cross-phase pass)

1. **Architectural coherence**: do the parts of this slice fit together
   cleanly with what came before and after? Look for: type names that
   drifted, helper functions duplicated across modules that should be
   shared, public-API shapes that changed without bumping a version.
2. **Cross-phase invariant violations**: do later-phase commits subtly
   break earlier-phase invariants? E.g. did a perf optimization in A6.5
   weaken a correctness guarantee from A4? Did a feature flag added in
   A-HW change behavior that A3's tests assumed?
3. **Tech-debt accumulation**: every `// TODO`, `// FIXME`, `// XXX`,
   `unimplemented!()`, `unreachable!()`, `#[allow(...)]`, `#[cfg(...)]`
   gate that's still in scope. Are any stale (the gating condition now
   always fires one way)? Are any silently shipping a deferred fix?
4. **Test pyramid balance**: unit tests vs integration vs property/fuzz
   vs end-to-end. Is the part's test surface meaningful or rubber-
   stamping? Look for: tests asserting on internal state instead of
   observable contracts; tests that would pass with the function body
   replaced by `Ok(())`; large `_ =>` fall-through arms that hide
   coverage gaps.
5. **Observability completeness**: every counter listed in spec §9.1
   the part claims to wire — is it incremented at every relevant code
   path? Every event in §9.2 — emitted in the right ordering? Counters
   declared but never written are a defect; counters written from a
   single site are a smell.
6. **Memory ordering / ARM portability**: any `Ordering::Relaxed` on a
   counter that another thread reads for ordering decisions? Any
   x86_64-only assumption (cache line size, atomic-fence semantics,
   non-aligned-load tolerance) baked into a struct layout or unsafe
   block?
7. **C-ABI stability**: any cbindgen-emitted symbol that changed shape
   between phases without explicit deprecation? Any header-side
   mismatch with the Rust source of truth? Verify via the
   crates/dpdk-net/cbindgen.toml + the generated header in target/ if
   available.
8. **Hidden coupling**: any module pair that now know about each
   other's internals beyond the documented interface? Look for:
   `pub(crate)` items reached from across module boundaries,
   `impl Foo for Bar` blocks that violate the spec's "Bar doesn't
   know about Foo" contract.
9. **Documentation drift**: doc comments referencing functions/types
   that have since been renamed; module-level docs describing an
   architecture that no longer matches code.

## Output format

Emit your review at:
docs/superpowers/reviews/cross-phase-retro-part-<N>-claude.md

Schema:

# Part <N> Cross-Phase Retro Review (Claude)

**Reviewer:** superpowers:code-reviewer subagent (opus 4.7)
**Reviewed at:** <date>
**Part:** <N> — <theme>
**Phases:** <list>

## Verdict

CLEAN | MINOR-ISSUES | NEEDS-FOLLOWUP | NEEDS-FIX

## Architectural drift

<issue per bullet, with file:line citations>

## Cross-phase invariant violations

<issue>

## Tech debt accumulated

<TODO/FIXME/allow/etc with rationale for whether to clear, defer, or document>

## Test-pyramid concerns

<gaps + rubber-stamping flags>

## Observability gaps

<wired-but-never-incremented counters, missing emit sites>

## Memory-ordering / ARM-portability concerns

<x86-isms, suspicious Relaxed reads>

## C-ABI / FFI

<symbol drift, header mismatches>

## Hidden coupling

<module-pair concerns>

## Documentation drift

<stale doc-comments, renamed-but-not-updated references>

## FYI / informational

<things future maintainers should know but not worth fixing now>

## Verification trace

<files read, greps run>

## Working notes

Be SPECIFIC: every finding must cite file:line. No "the test file looks
thin" — quote the test name + what's missing. No "this could be
better" — say what's broken now and why.

YAGNI on findings: don't propose architectural rewrites. Flag what's
broken, not what's possible to do better.

Don't re-flag what the per-phase mTCP/RFC reviews already caught and
documented. If a finding has a citation in
docs/superpowers/reviews/phase-aN-*.md, skip it.
```

---

## Reusable Codex reviewer prompt

Dispatch via the `codex:codex-rescue` subagent. Codex is stronger at mechanical
defect detection (lock-order, atomic semantics, leak edges, off-by-one) so the
prompt biases toward those.

```
You are doing a cross-phase retrospective review of Stage 1 work in a
Rust+DPDK userspace TCP stack. Per-phase compliance reviews already
happened. Your job is to catch what those reviews missed, with a
specific bias toward MECHANICAL defects: arithmetic edges, atomic /
memory-ordering nuances, lock acquisition ordering, leak edges,
unsafe-block invariants, error-path correctness. Architectural
analysis is being done by a parallel Claude reviewer — don't duplicate
that work.

## Part under review

<paste the part brief from below>

## Phase-scoped commits

`git log --oneline <prev-phase-tag>..<this-part-final-tag>` — read
each commit's diff and inspect the touched code.

## Review focus

1. **Arithmetic edges**: integer overflow / underflow on u32/u16
   wraparound (TCP seq/ack), saturation casts, signed-vs-unsigned
   compares, divisions that can divide-by-zero on a degenerate input
   (e.g. zero-RTT samples, zero-cwnd, empty-window).
2. **Atomic / memory ordering**: every `AtomicU64`/`AtomicU32`/
   `AtomicBool` op — is the ordering (`Relaxed`/`Acquire`/`Release`/
   `AcqRel`/`SeqCst`) correct for what it's protecting? `Relaxed` on
   a counter is fine; `Relaxed` on a flag another thread reads for
   ordering is a bug. Look for `.load(Relaxed)` immediately followed
   by a read of another field — classic happens-before violation.
3. **Lock ordering**: every `RefCell::borrow_mut` + nested borrow,
   every `Mutex::lock` chain. Find any inconsistent acquisition
   order across two paths through the engine. The DPDK single-lcore
   RTC model means deadlock isn't possible from concurrency, but a
   re-entrant `borrow_mut` on the same RefCell panics — that IS a
   bug.
4. **Mempool / mbuf leak edges**: every `rte_pktmbuf_alloc` →
   `rte_pktmbuf_free` pair. Every `MbufHandle` clone / drop. Every
   error path that returns early — does it return the mbuf? Per
   PR #9 cliff fix this is a hot area; look for any new leak edges
   added since.
5. **Unsafe invariants**: every `unsafe` block — what's the safety
   contract documented in the doc-comment? Does the actual code
   meet it? Look for: aliased mutable refs through raw pointers,
   `Box::from_raw` without matching `Box::into_raw`, `MaybeUninit`
   without proper init, `transmute` between non-equivalent types.
6. **Error-path correctness**: every `?` and every explicit
   `match`/`if let Err`. Does the error path drop / clean up
   resources owned by the function? Are partially-constructed
   states left observable to other code (e.g. a connection in the
   flow_table that was inserted before the error happened and not
   removed)?
7. **TCP-spec edges**: SEQ/ACK arithmetic on u32 wraparound (RFC
   9293's `SND.UNA <= ACK <= SND.NXT` is a modular comparison, not
   a numeric one); window-edge handling at the wrap boundary;
   payload-length signed/unsigned mistakes that a 32-bit window
   could trigger.
8. **Timer wheel + timer ordering**: every `timer_wheel.add`,
   `timer_wheel.cancel`. Are cancellations on connection-tear-down
   guaranteed? Are there timers that can fire after a connection
   is removed from the flow table?
9. **Counter increment placement**: every `counter.fetch_add(1,
   ...)` — is it inside the conditional that justifies the
   increment, or outside it (so it fires on the unrelated branch)?
   Inverted/swapped counters between paths.

## Output format

Emit your review at:
docs/superpowers/reviews/cross-phase-retro-part-<N>-codex.md

Same schema as the Claude reviewer (Verdict, sections, citations,
verification trace). Use the SAME section headers so the synthesis
step can diff the two reviews row-for-row.

Each finding MUST cite file:line. Each finding MUST classify itself
as one of: BUG (definitely broken), LIKELY-BUG (probably broken,
needs verification), SMELL (concerning pattern, may not be a bug),
FYI (informational only). Don't soft-classify a BUG as SMELL.

If you find a finding the per-phase reviews already documented (cited
in docs/superpowers/reviews/phase-aN-*.md), skip it.
```

---

## Per-part briefs

Paste the appropriate brief into both the Claude and Codex prompts before
dispatching. Replace `<prev-phase-tag>` and `<this-part-final-tag>` with the
actual git tags listed under "Tags" for each part.

```
## PART 1 — A1, A2 (foundation)

Phases: A1 (crate skeleton + EAL bring-up + cbindgen), A2 (L2/L3 RX/TX,
ARP, ICMP, PMD wrapper).
Tags: phase-a1-complete, phase-a2-complete.
Specs: docs/superpowers/specs/2026-04-17-dpdk-tcp-design.md §1-§3, §5.
Plans: docs/superpowers/plans/2026-04-17-stage1-phase-a1-skeleton.md,
       docs/superpowers/plans/2026-04-17-stage1-phase-a2-l2-l3.md.
Reviews already done: phase-a1-mtcp-compare.md (if exists),
phase-a2-mtcp-compare.md.
Files in scope:
  - crates/dpdk-net-sys/  (entire crate)
  - crates/dpdk-net-core/src/{eal,l2_eth,l3_ip,arp,icmp,flow_table}.rs
  - crates/dpdk-net-core/src/engine.rs (only the bring-up + poll_once
    skeleton; the TCP path lands in A3)
  - crates/dpdk-net/  (cbindgen scaffolding)
Cross-phase concerns specific to this part:
  - Has anything in the EAL bring-up sequence been added/reordered
    since A1 that the spec §3 docs don't reflect?
  - ARP / ICMP paths: are the gateway-resolution timeouts still as
    A2 documented?
```

```
## PART 2 — A3, A4 (TCP FSM core + options + reassembly + SACK)

Phases: A3 (TCP FSM, basic data path, send/recv/close), A4 (options
encoder/decoder including window scale + timestamps + SACK + MSS,
PAWS, reassembly scoreboard, SACK scoreboard).
Tags: phase-a3-complete, phase-a4-complete.
Specs: docs/superpowers/specs/2026-04-17-dpdk-tcp-design.md §6.1, §6.3,
       §7. docs/superpowers/specs/2026-04-18-stage1-phase-a3-tcp-basic.md,
       docs/superpowers/specs/2026-04-18-stage1-phase-a4-options-paws-
       reassembly-sack.md.
Plans: 2026-04-18-stage1-phase-a3-tcp-basic.md,
       2026-04-18-stage1-phase-a4-options-paws-reassembly-sack.md.
Files in scope:
  - crates/dpdk-net-core/src/{tcp_state,tcp_input,tcp_output,
    tcp_options,tcp_reassembly,tcp_sack,tcp_conn}.rs
  - crates/dpdk-net-core/src/engine.rs (TCP-relevant blocks — FSM
    transition, send, recv, close)
  - crates/dpdk-net-core/tests/* covering options encode/decode,
    reassembly, SACK, FSM transitions
Cross-phase concerns:
  - SEQ/ACK u32 wraparound at every comparison site.
  - PAWS timestamp expiry interaction with the engine's monotonic
    clock — has the clock source changed since A4?
```

```
## PART 3 — A5, A5.5, A5.6 (loss recovery + observability)

Phases: A5 (RACK reorder detection, RTO, TLP, retransmit, ISS), A5.5
(internal-event log forensics + per-packet events + TLP tuning), A5.6
(RTT histogram).
Tags: phase-a5-complete, phase-a5-5-complete, phase-a5-6-complete.
Specs: 2026-04-18-stage1-phase-a5-rack-rto-retransmit-iss-design.md,
       2026-04-18-stage1-phase-a5-5-event-log-forensics-design.md,
       2026-04-19-stage1-phase-a5-6-rtt-histogram-design.md.
Files in scope:
  - crates/dpdk-net-core/src/{tcp_rack,tcp_rto,tcp_tlp,tcp_retrans,
    tcp_events,tcp_rtt_hist}.rs (or wherever they ended up)
  - crates/dpdk-net-core/src/engine.rs (timer wheel + retransmit
    fire handlers)
Cross-phase concerns:
  - Timer-wheel cancel discipline on tear-down — any leftover timers?
  - `tcp_per_packet_events` feature gating: every emit site checked,
    or are some unconditional?
  - RTT histogram bucketing: does it survive ARM (per the auto-memory
    portability rule)?
  - `obs.events_dropped` overflow path: how does the soft-cap behave
    under sustained event pressure?
```

```
## PART 4 — A6, A6.5, A6.6, A6.7 (public API + zero-copy + FFI safety)

Phases: A6 (public API completeness — events, recv, send, close, error
codes), A6.5 (hot-path allocation elimination — Vec::with_capacity vs.
reused buffers, smallvec sizing), A6.6 (RX zero-copy via mbuf
borrow-views), A6.7 (FFI safety audit — every unsafe block + every
cbindgen symbol).
Tags: phase-a6-complete, phase-a6-5-complete, phase-a6-6-7-complete.
Specs: 2026-04-19-stage1-phase-a6-public-api-completeness-design.md,
       2026-04-20-stage1-phase-a6-5-hot-path-alloc-elimination-design.md,
       2026-04-20-stage1-phase-a6-6-and-a6-7-rx-zero-copy-and-ffi-safety-
       audit-design.md, 2026-04-20-stage1-phase-a6-6-7-fused-design.md.
Files in scope:
  - crates/dpdk-net/src/{lib,api}.rs and the cbindgen header
  - crates/dpdk-net-core/src/{api,events_queue,mbuf_handle}.rs
  - tests/ffi-test/  (entire crate)
Cross-phase concerns:
  - Hot-path alloc elimination: do any Vec::new() / Box::new() still
    appear inside engine.rs's poll_once or rx_frame paths?
  - mbuf borrow-views: is every `MbufHandle` clone/drop exhaustively
    audited for refcount balance? PR #9 was specifically about a
    cliff bug in this area; have any new edges been added since?
  - cbindgen symbol stability: every `pub extern "C" fn` matches
    the .h header exactly?
```

```
## PART 5 — A-HW, A-HW+ (ENA HW offloads)

Phases: A-HW (ENA HW offload integration — TX cksum, RX cksum, RSS,
mbuf fast-free, RX timestamp), A-HW+ (offload knobs + observability +
runtime disable).
Tags: phase-a-hw-complete, phase-a-hw-plus-complete.
Specs: 2026-04-19-stage1-phase-a-hw-ena-offload-design.md,
       2026-04-20-stage1-phase-a-hw-plus-ena-obs-knobs.md.
Files in scope:
  - crates/dpdk-net-core/src/{eal,offloads,l3_ip,l2_eth}.rs (offload
    code paths)
  - crates/dpdk-net-core/Cargo.toml (the hw-offload-* feature flags)
  - tests/* covering offload-on vs offload-off behavioral parity
Cross-phase concerns:
  - Feature-flag matrix: is every `cfg!(feature = "hw-offload-*")`
    branch tested for both states?
  - The runtime `tx/rx_cksum_offload_active` private fields on
    Engine — is the documented spec/§6.3 mention of "feature
    determines behavior" still accurate, or has runtime negotiation
    crept in?
  - RX timestamp dynfield: alignment + ARM portability of the
    dynfield offset.
```

```
## PART 6 — A7, A8, A8.5 (test infrastructure)

Phases: A7 (loopback test server + packetdrill shim), A8 (tcpreq +
observability gate — assert-exact counter values), A8.5 (test
coverage expansion).
Tags: phase-a7-complete, phase-a8-complete, phase-a8-5-complete.
Specs: 2026-04-21-stage1-phase-a7-loopback-test-server-packetdrill-
       shim-design.md, 2026-04-22-stage1-phase-a8-tcpreq-observability-
       gate-design.md, 2026-04-23-a8.5-test-coverage-expansion-design.md.
Files in scope:
  - tools/packetdrill-shim-runner/, tools/tcpreq-runner/
  - crates/dpdk-net-core/src/test_server.rs (the test-server cargo
    feature)
  - crates/dpdk-net-core/tests/* (everything)
Cross-phase concerns:
  - Test-server cargo feature: every public-API surface gated under
    it that should be — and nothing accidentally exposed in the
    production build?
  - Observability gate's assert-EXACT counter values: are they
    still passing after A10's perf work? Any drift in the counter-
    value contract?
  - Test coverage: per the test-pyramid rule — are A8.5's added
    tests actually meaningful, or rubber-stamping?
```

```
## PART 7 — A9 (proptest + fuzz + FaultInjector)

Phases: A9 (property tests via proptest, libfuzzer/cargo-fuzz targets,
FaultInjector RX-side middleware).
Tag: phase-a9-complete.
Specs: 2026-04-21-stage1-phase-a9-property-fuzz-faultinjector-design.md.
Files in scope:
  - crates/dpdk-net-core/src/fault_injector.rs
  - crates/dpdk-net-core/fuzz/ (libfuzzer targets — sub-Cargo project)
  - crates/dpdk-net-core/tests/fault_injector_*.rs
  - tools/scapy-fuzz-runner/
Cross-phase concerns:
  - FaultInjector feature gating: zero overhead in default builds?
    Verify by reading the engine's RX entry point for any
    unconditional `if cfg!` branch.
  - Fuzz corpora: are there `// TODO: add input X` notes that have
    been outstanding for >2 phases?
  - Property tests: any `#[ignore]` marker that should be cleared?
```

```
## PART 8 — A10 (AWS infra + benchmark harness + microbench + DPDK 24.x)

Phases: A10 (AWS bench-pair fleet via resd-aws-infra, bench-e2e
RTT harness, bench-stress netem matrix, bench-vs-linux, bench-vs-mtcp,
bench-offload-ab, bench-obs-overhead, bench-micro criterion targets,
DPDK 23.11 → 24.x adopt + perf optimizations cherry-picked to master).
Tag: phase-a10-complete (and the deferred-fixes work merged via PR #9).
Specs: 2026-04-21-stage1-phase-a10-aws-infra-setup.md,
       2026-04-21-stage1-phase-a10-benchmark-harness.md,
       2026-04-23-a10-microbench-perf-and-dpdk24-adopt-design.md,
       2026-04-29-a10-deferred-fixes-design.md.
Files in scope:
  - tools/bench-{e2e,stress,vs-linux,vs-mtcp,offload-ab,obs-overhead,
    micro,common,ab-runner,report,rx-zero-copy}/
  - scripts/{bench-nightly.sh,check-bench-preconditions.sh,
    bench-ab-runner-gdb.sh}
  - crates/dpdk-net-core changes for DPDK 24.x compat + perf
    cherry-picks (the recent reflog entries on master)
Cross-phase concerns:
  - DPDK 24.x adopt: every API change handled, or compile-time-
    only verified? Run a quick `cargo build` against the current
    DPDK to confirm.
  - The ~14 perf cherry-picks landed on master since PR #9 — do
    any of them weaken correctness invariants from earlier phases?
  - bench-nightly.sh: does the orchestrator still work end-to-end,
    or has any of A10.5's work (or unrelated drift) broken it?
  - The PR #9 cliff fix (`MbufHandle::Drop` switching from
    `rte_mbuf_refcnt_update(-1)` to `rte_pktmbuf_free_seg`) — has
    any code path SINCE re-introduced the original pattern?
```

```
## PART 9 — A10.5 (Layer H correctness gate)

Phase: A10.5 (landed via PR #10).
Tag: phase-a10-5-complete.
Specs: 2026-05-01-stage1-phase-a10-5-layer-h-correctness-design.md.
Plan: 2026-05-01-stage1-phase-a10-5-layer-h-correctness.md.
Files in scope:
  - tools/layer-h-correctness/  (entire new crate)
  - scripts/layer-h-{smoke,nightly}.sh
  - The four bench-stress / bench-nightly.sh files touched by the
    bundled bug fix in commit ce6fc24 / 9d65e37
Cross-phase concerns:
  - The forwarder feature pattern (T8 fix-up): is it consistent
    with how other layer-h-correctness consumers should configure
    feature flags?
  - The disjunctive corruption-counter assertion (row 14): does it
    actually fire under both offload-on and offload-off bench-pair
    builds, or has only one path been exercised?
  - The newly added PR #9 RX-leak side-checks: are they meaningful
    in practice, or always-pass under the smoke set's workload
    intensity?
  - The post-merge `gate layer-h-correctness behind test-server
    feature (gateway ARP regression)` fix on master (commit
    8147404 if still present) — what was the regression, and is
    there a missing test that would have caught it earlier?
```

---

## Synthesis pass (after each part)

Once both reviewers report, dispatch this small synthesis subagent (Claude,
model=`opus`, subagent_type=`general-purpose`):

```
You are merging two independent retrospective code reviews of Stage 1
Phase work for Part <N>. Both reviews used the same schema; your job
is to produce a unified findings list that:

1. **De-duplicates**: identical findings from both reviewers collapse
   to one entry, citing both as sources.
2. **Triages disagreement**: if Claude flagged a finding as SMELL and
   Codex flagged it as BUG, mark the entry as DISPUTED and escalate
   for human decision.
3. **Distinguishes blast radius**: separate "this needs a fix before
   A11 starts" from "this is a Stage 2 follow-up" from "FYI for
   future maintainers".

## Inputs
- Claude review: docs/superpowers/reviews/cross-phase-retro-part-<N>-claude.md
- Codex review:  docs/superpowers/reviews/cross-phase-retro-part-<N>-codex.md

## Output
docs/superpowers/reviews/cross-phase-retro-part-<N>-synthesis.md

Sections:
- BLOCK A11 (must-fix before next phase)
- STAGE-2 FOLLOWUP (real concern, deferred)
- DISPUTED (reviewer disagreement)
- AGREED FYI (both reviewers flagged but not blocking)
- INDEPENDENT-CLAUDE-ONLY (only Claude flagged; rate plausibility)
- INDEPENDENT-CODEX-ONLY (only Codex flagged; rate plausibility)
```

---

## Driver workflow

For each part 1 → 9:

1. Paste the reusable Claude prompt + the part-N brief → dispatch via the
   `superpowers:code-reviewer` subagent (model=`opus`).
2. Paste the reusable codex prompt + the same part-N brief → dispatch via the
   `codex:codex-rescue` subagent.
3. Both run in parallel; each writes its review to disk under
   `docs/superpowers/reviews/cross-phase-retro-part-<N>-<reviewer>.md`.
4. Dispatch the synthesis prompt → unified report at
   `docs/superpowers/reviews/cross-phase-retro-part-<N>-synthesis.md`.
5. Read the synthesis. If it contains BLOCK A11 items, fix them before
   moving to the next part. Keep STAGE-2 FOLLOWUP items in a running
   `cross-phase-retro-stage2-followup.md` index for later.
6. Move to part N+1.

After all 9 parts, do a meta-synthesis (manual): scan the 9 synthesis reports,
look for patterns that recur across parts (e.g. "Claude consistently flagged X
across parts 2/4/8" — that's an indication of a pervasive issue, not a
local one). Capture pervasive patterns in a final summary at
`docs/superpowers/reviews/cross-phase-retro-summary.md`.

---

## Operator notes

- Use opus 4.7 for every Claude dispatch (per project memory).
- Run codex via the `codex:codex-rescue` subagent type. The codex helper
  reads `~/.codex/` config; verify the codex CLI is set up before starting
  via the `codex:setup` skill if uncertain.
- Per-task two-stage review discipline does NOT apply here — this is a
  retrospective review, not new implementation. The implementation reviews
  already happened during each phase.
- Keep each part's review reports in git. They become the audit trail for
  why specific Stage-2 followups exist.
- If a finding turns out to be in scope for A11 (the next phase), file it
  under A11's plan rather than fixing inline — the goal is to catch issues,
  not to do A11 work in disguise.
- Time budget per part: ~15–25 min wall clock (subagent execution + your
  reading time). Across 9 parts: ~3 hours of focused work. Spread across a
  day with breaks; reading the synthesis between parts is the main cognitive
  load.
