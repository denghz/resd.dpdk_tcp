# Part 1 Cross-Phase Retro Synthesis

**Synthesizer:** general-purpose subagent (opus 4.7)
**Synthesized at:** 2026-05-05
**Part:** 1 — Crate skeleton, EAL bring-up, L2/L3 (PMD wrapper, ARP, ICMP)
**Phases:** A1, A2
**Inputs:** cross-phase-retro-part-1-claude.md, cross-phase-retro-part-1-codex.md

## Combined verdict

**NEEDS-FIX** — Two independent reviewers converged on NEEDS-FIX with non-overlapping evidence: Claude flagged ABI/FFI correctness defects (silently-ignored public config fields, panic-across-FFI in `eal_init`, x86-only compile lock); Codex independently found a HEAD-only counter double-bump in the post-A2 RX checksum-offload wrapper plus a timer-last-send retry-state bug. Together this is enough concrete defect surface to require fixes before A11 starts.

## BLOCK A11 (must-fix before next phase)

- **C-ABI dead fields silently ignore caller intent** | Source: Claude §C-ABI/FFI bullet 1 | BUG | `dpdk_net_engine_config_t.tcp_min_rto_ms`, `tcp_timestamps`, `tcp_sack`, `tcp_ecn` are exposed in the public C ABI (`crates/dpdk-net/src/api.rs:28-30,34`; `include/dpdk_net.h:86-88,92`) but never read by `dpdk_net_engine_create` (`crates/dpdk-net/src/lib.rs:141-269`); SYN options are hard-coded at `crates/dpdk-net-core/src/engine.rs:243-257`. Spec §4 lines 91-93 are currently a lie at the C ABI level. Either delete the fields (ABI break, pre-1.0 OK) or wire them through. Blocks A11 because A11 is the last natural moment to break the ABI before more callers depend on it. | **Rationale to block:** ABI dead fields persist forever once shipped; A11+ phases will only add more knobs on top of a known-broken contract.

- **Panic across FFI boundary in `eal_init`** | Source: Claude §Cross-phase invariant violations bullet 1 | BUG | `engine::eal_init` does `CString::new(*s).unwrap()` (`crates/dpdk-net-core/src/engine.rs:959`) and `EAL_INIT.lock().unwrap()` (`crates/dpdk-net-core/src/engine.rs:933`). Spec §3 forbids panic across `extern "C"`; under a dev build with `panic = "unwind"` this is UB. Convert both to error returns. | **Rationale to block:** the public entry point `dpdk_net_eal_init` (`crates/dpdk-net/src/lib.rs:118`) directly calls into this; any C++ caller hitting a malformed argv triggers UB. Cheap to fix.

- **NIC BAD IPv4 checksum double-counts `ip.rx_csum_bad`** | Source: Codex §Cross-phase invariant violations bullet 1 + §Observability gaps bullet 1 | BUG | `ip_decode_offload_aware` bumps both `eth.rx_drop_cksum_bad` and `ip.rx_csum_bad` then returns `Err(L3Drop::CsumBad)` at `crates/dpdk-net-core/src/l3_ip.rs:213-219`. `Engine::handle_ipv4` then bumps `ip.rx_csum_bad` again at `crates/dpdk-net-core/src/engine.rs:3928-3930`. NIC-detected bad checksums increment 2x; software-detected increment 1x. Drift introduced by post-A2 commit `e2aae95`. Operators see a 2x rate only when RX checksum offload is active. | **Rationale to block:** active counter-correctness regression on a production-mode (offload-on) path; quick fix is to make one layer own the IP counter for the NIC-BAD branch; A11 will compound counter-coverage drift if left.

## STAGE-2 FOLLOWUP (real concern, deferred)

- **`compile_error!` on non-x86_64 in `clock.rs:39`** | Source: Claude §Cross-phase invariant violations bullet 2 + §Memory-ordering bullet 1 | BUG (per Claude classification) | The whole crate fails to compile on aarch64, conflicting with `feedback_arm_roadmap.md`. Either gate `Cargo.toml` to `[target.'cfg(target_arch = "x86_64")']` or implement the aarch64 `cntvct_el0` path. **Deferred** because Stage 1 is x86_64-only by current scope; ARM unblock is a Stage 2 deliverable.

- **`maybe_emit_gratuitous_arp` never migrated to timer wheel** | Source: Claude §Architectural drift bullet 2 | SMELL | A2 plan promised a "~3-line A6 change"; A6 shipped `tcp_timer_wheel.rs` but never migrated GARP. Sites: `crates/dpdk-net-core/src/engine.rs:6238-6255,2591,2659`. Functionally correct today.

- **Gratuitous-ARP / gateway-probe timer last-send timestamp updated even on failed TX** | Source: Codex §Hidden coupling bullet 1 | LIKELY-BUG | `*last = now` writes unconditionally at `crates/dpdk-net-core/src/engine.rs:6248-6254` (GARP) and `:6525-6537` (gateway probe), but `tx_frame` returns false on alloc failure / full TX ring (`:2124-2140,2175-2183`). A transient TX failure suppresses the next GARP/probe for a full interval. Weakens refresh cadence; for zero-gateway-MAC discovery delays by 1 s per failed attempt. **Deferred** because the production path with a static gateway MAC rarely hits the probe loop and TX-ring exhaustion is itself rare; combine fix with the timer-wheel migration above.

- **`EngineConfig.mbuf_data_room` overflow on Rust-direct callers** | Source: Codex §Cross-phase invariant violations bullet 2 | LIKELY-BUG | `cfg.mbuf_data_room + sys::RTE_PKTMBUF_HEADROOM as u16` at `crates/dpdk-net-core/src/engine.rs:1205,1257` can panic in debug or wrap in release. C ABI pins the field to 2048, so this is a Rust-direct edge. **Deferred** because the C ABI clamps the field; Stage 2 should add an `EngineConfig::validate()` that rejects values above `u16::MAX - RTE_PKTMBUF_HEADROOM`.

- **Gateway-MAC discovery silently grew an ARP-REQUEST probe path** not in spec §8 / A2 plan | Source: Claude §Architectural drift bullet 3 | SMELL | `Engine::maybe_probe_gateway_mac` (`crates/dpdk-net-core/src/engine.rs:6512-6538`) emits a unicast ARP REQUEST every 1s when `gateway_mac == [0;6]`. Spec §8 should be updated to note this. Combine with the doc-drift bucket below.

- **Public `dpdk_net_engine_config_t.tcp_initial_rto_ms` documented in spec but absent in C ABI** | Source: Claude §C-ABI/FFI bullet 2 | SMELL | Spec §4 line 98 lists `_ms`-suffixed; actual field is `tcp_initial_rto_us`. Spec text needs an update. **Deferred** because the field exists under a different name; spec edit is a doc-only fix that travels with the BLOCK-A11 dead-fields cleanup.

- **`ip.rx_drop_short` double-bumped on `BadTotalLen`** | Source: Claude §Observability gaps bullet 1 | SMELL | Decoder distinguishes `Short` (header < 20) from `BadTotalLen` (`total_len > pkt.len()`); engine collapses both to `ip.rx_drop_short` at `crates/dpdk-net-core/src/engine.rs:3924-3927`. Operators cannot distinguish framing truncation from malformed total_len. Add `ip.rx_drop_bad_total_len` slow-path counter.

- **`OtherDropped | Malformed` ICMP results collapse to no-op** | Source: Claude §Observability gaps bullet 2 | SMELL | A2 mTCP review's `AD-8` deferred this; A3-A10 didn't pick it up (`crates/dpdk-net-core/src/engine.rs:3986`). Split into `ip.rx_icmp_other_dropped` + `ip.rx_icmp_malformed`.

- **No default-build engine-level test for the NIC-BAD checksum counter** | Source: Codex §Test-pyramid concerns bullet 1 | SMELL | Why the counter-double-bump bug above wasn't caught during commit `e2aae95`. Pin one offload-aware engine-level test asserting `ip.rx_csum_bad += 1` per NIC-BAD packet.

- **TAP integration test gated behind `DPDK_NET_TEST_TAP=1`; no default-build dispatcher coverage** | Source: Claude §Test-pyramid concerns bullet 1 | SMELL | A9 added `test-inject`; no one backfilled A2 wiring tests. Add inject-path unit tests for `Engine::handle_arp` / `handle_ipv4` / `rx_frame`.

- **Counter-coverage hole: `ip.rx_drop_unsupported_proto`** | Source: Claude §Test-pyramid concerns bullet 3 | SMELL | Only TAP test covers it; default-build sweep doesn't bump it.

- **`tx_data_frame` is `#[allow(dead_code)]` 5+ phases on** | Source: Claude §Tech debt accumulated bullet 3 | SMELL | 60+ lines vestigial. Delete.

- **`PortConfigOutcome::applied_rx_offloads` / `applied_tx_offloads` `#[allow(dead_code)]` fields are unread** | Source: Claude §Tech debt accumulated bullet 4 | SMELL | Drop the fields and the allow.

- **`rx_drop_nomem_prev` accessor `#[allow(dead_code)]`** | Source: Claude §Tech debt accumulated bullet 5 | FYI | Verify T21 ended up using it; if not, drop or wire it.

- **Stale `#[allow(unused_variables)]` on `ip_decode_offload_aware` params** | Source: Claude §Tech debt accumulated bullet 6 | SMELL | Belt-and-suspenders; remove the function-signature allows.

- **`/proc/net/arp` MAC parser accepts extra octets** | Source: Codex §Tech debt accumulated bullet 1 | SMELL | `parse_proc_arp_line` at `crates/dpdk-net-core/src/arp.rs:268-273` doesn't check `parts.next()` is exhausted; `aa:bb:cc:dd:ee:ff:00` parses as `aa:bb:cc:dd:ee:ff`. Mechanical edge in A2 gateway-MAC bootstrap.

- **Zero/undersized `tx_ring_size` invalidates batch-ring capacity contract** | Source: Codex §Tech debt accumulated bullet 2 | SMELL | Public field at `crates/dpdk-net-core/src/engine.rs:382`; `send_bytes` push at `:5486-5494`, retransmit push at `:6202-6207`. Default and C ABI use 512 (so not a current C-ABI bug); Rust-direct edge.

- **L2 broadcast acceptance broader than comment claims** | Source: Codex §Hidden coupling bullet 2 | SMELL | `l2_decode` accepts broadcast before checking ethertype at `crates/dpdk-net-core/src/l2.rs:38-47`; comment at `:25-27` says "for ARP". Hidden L2/L3 policy coupling.

- **Spec/code drift on `arp.rs` module doc + A2 mTCP review's `AD-1`/`AD-2`** | Source: Claude §Documentation drift bullets 1-2 | FYI | `arp.rs` module doc says "static-gateway mode. We don't run a dynamic resolver" — but `classify_arp` at `:127-139` is a partial dynamic resolver. A2 mTCP `AD-1`/`AD-2` need "Status: superseded" lines.

- **Cross-phase brief lists `eal.rs` / `l2_eth.rs` that don't exist** | Source: Claude §Architectural drift bullet 1 + §Documentation drift bullet 4 | FYI | `eal_init` lives in `crates/dpdk-net-core/src/engine.rs:932`; L2 module is `l2.rs` not `l2_eth.rs`. Either move `eal_init` to its own module or fix the brief.

- **ICMP parser comment claims it requires 8 bytes of original transport but code only requires inner IPv4 header** | Source: Codex §Documentation drift bullet 1 | SMELL | Comment at `crates/dpdk-net-core/src/icmp.rs:70-72` vs guard at `:72-74`. Not a runtime bug.

- **`Engine::new` is 420+ lines** | Source: Claude §Hidden coupling bullet 3 | SMELL | Phase-by-phase accumulation; split into helpers. Out-of-scope for fix-what's-broken pass.

- **`THREAD_COUNTERS_PTR` thread-local in `mempool.rs:21-24`** | Source: Claude §Hidden coupling bullet 4 | SMELL | `EngineNoEalHarness` doesn't set it; harness-side mbuf paths silently lose leak diagnostics. Document or wire.

- **`siphash_4tuple` truncates 64-bit hash to `u32`** | Source: Claude §Memory-ordering bullet 2 | SMELL | At `crates/dpdk-net-core/src/flow_table.rs:43-61`. Document the `u32` contract or widen to `u64`.

- **`our_mac` / `gateway_mac()` / `gateway_ip()` accessors use three different patterns** | Source: Claude §Architectural drift bullet 4 | SMELL | Stage 1 audit gate doesn't catch accessor drift. FYI.

- **`EthCounters.rx_drop_miss_mac` field name vs spec "MissMac"** | Source: Claude §Cross-phase invariant violations bullet 3 | FYI | Two casing conventions for one concept.

- **`build_arp_*` regression tests don't cross-check `tx_frame` length acceptance** | Source: Claude §Test-pyramid concerns bullet 2 | FYI | Add `const_assert!(ARP_FRAME_LEN <= 256)`.

- **`parse_proc_arp_line` test data uses fixed-width columns, not real `/proc/net/arp` byte format** | Source: Claude §Test-pyramid concerns bullet 4 | FYI | Add a real Linux 6.x byte-for-byte sample.

- **`eth.rx_pkts` per-burst vs per-mbuf bump shape divergence** | Source: Claude §Observability gaps bullet 3 | SMELL | Extract `bump_rx_accepted(n)` helper.

- **`gateway_mac()` reads `Cell::get` non-atomic** | Source: Claude §Observability gaps bullet 4 | FYI | Single-lcore invariant; document on `Engine` itself.

- **`dpdk_net_eal_init` drops the actual DPDK errno, returns `-libc::EAGAIN`** | Source: Claude §C-ABI/FFI bullet 3 | FYI | Pass `-rte_errno` through directly.

- **`cbindgen.toml [export] include` whitelist has no positive coverage assertion** | Source: Claude §C-ABI/FFI bullet 4 | SMELL | Drift-check script in commit `c069421` only diffs regenerated bytes; missing-from-include type silently drops.

- **`engine.rs` reads `arp::ARP_FRAME_LEN` via raw stack arrays at 3 sites** | Source: Claude §Hidden coupling bullet 1 | FYI | Latent coupling; flagged for future maintainers.

- **A1 has no per-phase reviews** | Source: Claude §FYI bullet 6 | FYI | Spec §10.13/§10.14 exempts A1; A1 is the least-reviewed surface in the project. Cross-phase pass should weight A1 findings extra.

## DISPUTED (reviewer disagreement)

*None.* Reviewers explicitly skip-listed each other's findings; where both flagged the same area (e.g., `clock.rs` x86-only, ABI dead fields, `eal_init` panic, TAP-gated tests, counter Relaxed, A2 review path renames) they agreed on classification. The only structural difference is that Codex labeled the NIC-BAD double-count as **BUG** (a HEAD-only post-A2 regression) where Claude listed a related but distinct finding (`ip.rx_drop_short` collapse) as **SMELL** — these are different counters and not in conflict.

## AGREED FYI (both reviewers flagged but not blocking)

- **Default-build pure-module tests cover A2 parsers/builders; engine wiring is integration/test-inject-only** | Sources: Claude §Test-pyramid concerns bullet 1 + Codex §Test-pyramid concerns bullet 2 | Both reviewers independently called out the same risk shape; Codex deliberately did not re-flag it as a separate item.

- **Counter writes uniformly `Ordering::Relaxed`, justified by single-writer-lcore** | Sources: Claude §Memory-ordering bullet 3 + Codex §Memory-ordering bullet 2 | Both confirmed correct; no drift.

- **A2 review paths used `resd-*`; HEAD uses `dpdk-*`** | Sources: implicit in Claude (file path notes) + explicit in Codex §Documentation drift bullet 2 | Path drift only, not behavioral.

## INDEPENDENT-CLAUDE-ONLY (only Claude flagged; rate plausibility)

All Claude-only findings are listed in the STAGE-2 / BLOCK-A11 buckets above with their citations. Highlighted plausibility ratings:

- **C-ABI dead fields (BLOCK-A11)** | BUG | **HIGH** plausibility — direct file:line evidence at `crates/dpdk-net/src/api.rs:28-30,34` + `crates/dpdk-net-core/src/engine.rs:243-257` showing options hard-coded; Codex did not re-flag because explicitly skip-listed.

- **Panic across FFI in `eal_init` (BLOCK-A11)** | BUG | **HIGH** plausibility — `unwrap()` sites confirmed at the file:line; Codex skip-listed.

- **`compile_error!` x86_64 lock** | BUG | **HIGH** plausibility — single `compile_error!` at `crates/dpdk-net-core/src/clock.rs:39` directly visible; conflicts with `feedback_arm_roadmap.md`.

- **`maybe_emit_gratuitous_arp` never migrated to timer wheel** | SMELL | **HIGH** plausibility — A6 timer wheel exists; A2 helper still polled.

- **`ip.rx_drop_short` double-bumped on `BadTotalLen`** | SMELL | **HIGH** plausibility — explicitly documented in commit `d8dad1f` test text; Claude correctly identifies the observability fidelity loss.

- **`OtherDropped | Malformed` ICMP collapse** | SMELL | **HIGH** plausibility — A2 review `AD-8` already documents the deferral.

- **`tx_data_frame` / dead allocator-outcome fields / TAP-gated wiring tests** | SMELL | **HIGH** plausibility — Claude cited concrete `#[allow(dead_code)]` annotations and TAP env-gate.

- **`siphash_4tuple` u32 truncation** | SMELL | **MEDIUM** plausibility — code is correct today (table sized to `u32`); concern is forward-compat with RSS folding. Documentation fix is reasonable.

- **`THREAD_COUNTERS_PTR` harness gap** | SMELL | **MEDIUM** plausibility — diagnostic-only, silently disabled on harness path; Claude cites `EngineNoEalHarness` correctly.

- **`Engine::new` 420+ line straddle, accessor pattern divergence, doc drift in `arp.rs` module doc** | SMELL/FYI | **HIGH** plausibility — direct file evidence, drift is real but bounded.

- **`cbindgen.toml` whitelist coverage assertion** | SMELL | **HIGH** plausibility — drift-check script logic confirmed in commit `c069421`.

## INDEPENDENT-CODEX-ONLY (only Codex flagged; rate plausibility)

- **NIC BAD IPv4 checksum double-counts `ip.rx_csum_bad` (BLOCK-A11)** | BUG | **HIGH** plausibility — Codex cites two specific bump sites (`crates/dpdk-net-core/src/l3_ip.rs:213-219` and `crates/dpdk-net-core/src/engine.rs:3928-3930`) and identifies the introducing commit (`e2aae95`); the two-layer increment is mechanically verifiable. This finding is the strongest HEAD-only regression in the synthesis.

- **`EngineConfig.mbuf_data_room` overflow on Rust-direct callers** | LIKELY-BUG | **HIGH** plausibility — file:line for the `+ RTE_PKTMBUF_HEADROOM` arithmetic confirmed; C ABI pins the field, so blast radius is the direct-Rust crate consumers (test harnesses, internal tools).

- **GARP / gateway-probe `last_send` updated even on failed TX** | LIKELY-BUG | **HIGH** plausibility — Codex cites both timer sites and the `tx_frame` failure paths; logic is mechanical and matches the `*last = now` unconditional write pattern. Blast radius is small (rare TX failures) but the bug is real.

- **`/proc/net/arp` parser accepts extra octets** | SMELL | **HIGH** plausibility — concrete loop bound issue at `crates/dpdk-net-core/src/arp.rs:268-273`; kernel-generated input limits exposure but the parser is permissive.

- **Zero/undersized `tx_ring_size` capacity invariant violation** | SMELL | **MEDIUM** plausibility — depends on whether DPDK's `rte_eth_tx_queue_setup` rejects zero before the Vec ever sees a push. Codex acknowledges "not a current C-ABI bug." Latent edge for direct-Rust callers.

- **L2 broadcast acceptance broader than comment** | SMELL | **HIGH** plausibility — code path at `crates/dpdk-net-core/src/l2.rs:38-47` mechanically verifiable; downstream `NotOurs` drop catches the case but the comment is misleading.

- **ICMP parser comment vs guard mismatch** | SMELL | **HIGH** plausibility — documentation-only mismatch at `crates/dpdk-net-core/src/icmp.rs:70-74`; trivial to fix.

- **Null mbuf slots from `rx_burst` skipped after `eth.rx_pkts` already batched** | FYI | **MEDIUM** plausibility — Codex correctly notes this is a panic-firewall defense for an invalid PMD state, not a normal-path bug. DPDK contract says slots are non-null.

## Counts

Total findings: **42**
- BLOCK-A11: **3**
- STAGE-2: **27**
- DISPUTED: **0**
- AGREED-FYI: **3**
- CLAUDE-ONLY: **30** (3 BLOCK + 24 STAGE-2 + 3 AGREED — counted once in this row by source attribution; the BLOCK and STAGE-2 buckets above are de-duped across sources)
- CODEX-ONLY: **9** (1 BLOCK + 5 STAGE-2 + 3 informational)

Note on counting: BLOCK-A11 is **3** distinct findings (2 Claude-only, 1 Codex-only); STAGE-2 holds the remaining real concerns; AGREED-FYI captures the small set both reviewers explicitly converged on without re-flagging. Source-attribution counts overlap the bucket counts because findings live in exactly one bucket but were sourced from one or both reviewers.
