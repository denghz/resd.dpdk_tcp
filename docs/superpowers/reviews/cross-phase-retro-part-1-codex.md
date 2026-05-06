# Part 1 Cross-Phase Retro Review (Codex)
Reviewer: codex:codex-rescue
Reviewed at: 2026-05-05
Part: 1 — Crate skeleton, EAL bring-up, L2/L3 (PMD wrapper, ARP, ICMP)
Phases: A1, A2

## Verdict

NEEDS-FIX

One observed BUG in the HEAD RX checksum-offload path double-counts NIC-reported bad IPv4 checksums. Two LIKELY-BUG items are mechanical edge cases around unvalidated mbuf data-room arithmetic and ARP timer retry state after failed TX. I did not re-flag the Claude review's ABI dead fields, panic-across-FFI, x86-only clock, timer-wheel migration, ICMP malformed-counter collapse, or architectural drift findings.

## Architectural drift

- None newly observed in scope beyond the skip-listed Claude review. I treated the crate rename (`resd-*` in A1/A2 commits, `dpdk-*` at HEAD) as path drift only, not a behavioral finding.

## Cross-phase invariant violations

- **BUG — NIC BAD IPv4 checksum packets increment `ip.rx_csum_bad` twice.** Drift introduced after A2 by commit `e2aae95` ("a-hw task 8: RX checksum ol_flags inspection — IP + TCP L4"). At HEAD, `ip_decode_offload_aware` handles `CksumOutcome::Bad` by bumping both `eth.rx_drop_cksum_bad` and `ip.rx_csum_bad`, then returns `Err(L3Drop::CsumBad)` at `crates/dpdk-net-core/src/l3_ip.rs:213-219`. The caller `Engine::handle_ipv4` treats every `L3Drop::CsumBad` the same and bumps `ip.rx_csum_bad` again at `crates/dpdk-net-core/src/engine.rs:3928-3930`. Software-detected checksum failures bump once; NIC-detected failures bump twice. The fix is to make one layer own the IP counter for the NIC-BAD branch, or return a distinct drop reason already counted by the offload wrapper.

- **LIKELY-BUG — `EngineConfig.mbuf_data_room` can overflow while constructing RX/TX data mempools.** The field is public `u16` at `crates/dpdk-net-core/src/engine.rs:383`, and direct Rust callers can set it above `u16::MAX - RTE_PKTMBUF_HEADROOM`. `Engine::new` computes `cfg.mbuf_data_room + sys::RTE_PKTMBUF_HEADROOM as u16` for RX at `crates/dpdk-net-core/src/engine.rs:1205` and TX data at `crates/dpdk-net-core/src/engine.rs:1257`. In debug this can panic during engine creation; in release it can wrap and pass a too-small `data_room_size` to DPDK. The C ABI path pins `mbuf_data_room` to 2048 at `crates/dpdk-net/src/lib.rs:206`, so this is a Rust-direct configuration edge, but it is still a public bring-up invariant with no validator.

Mechanical category coverage: arithmetic edges found above; atomic/memory-ordering, lock ordering, and mbuf leak/double-free categories had no additional cross-phase invariant violation beyond skip-listed findings.

## Tech debt accumulated

- **SMELL — `/proc/net/arp` MAC parsing accepts extra colon-separated octets.** `parse_proc_arp_line` fills six bytes by iterating `for b in &mut mac_bytes` and never checks that `parts.next()` is exhausted afterward at `crates/dpdk-net-core/src/arp.rs:268-273`. A malformed field like `aa:bb:cc:dd:ee:ff:00` parses as `aa:bb:cc:dd:ee:ff` instead of being rejected. `/proc/net/arp` is kernel-generated in the normal path, so this is not a high-probability production fault, but it is a mechanical parser edge in the A2 gateway-MAC bootstrap helper.

- **SMELL — zero/undersized direct Rust `tx_ring_size` can invalidate the batch-ring capacity contract.** `EngineConfig.tx_ring_size` is public at `crates/dpdk-net-core/src/engine.rs:382`, and the internal TX batch is created with exactly that capacity at `crates/dpdk-net-core/src/engine.rs:1509`. Later, if the ring is full, `send_bytes` drains and then unconditionally `push`es the current mbuf into the Vec at `crates/dpdk-net-core/src/engine.rs:5486-5494`; retransmit does the same at `crates/dpdk-net-core/src/engine.rs:6202-6207`. The default and C ABI use 512 (`crates/dpdk-net-core/src/engine.rs:555-556`, `crates/dpdk-net/src/lib.rs:204-205`), and real DPDK queue setup likely rejects zero, so this is not a current C-ABI bug. It is still a mechanical gap in the Rust-direct config surface because a zero-capacity Vec will allocate on push, violating the bounded batch-ring assumption.

## Test-pyramid concerns

- **SMELL — no unit coverage appears to pin the NIC-BAD checksum counter ownership.** The offload classifier has unit tests for enum classification at `crates/dpdk-net-core/src/l3_ip.rs:387-437`, and the engine maps `L3Drop::CsumBad` to `ip.rx_csum_bad` at `crates/dpdk-net-core/src/engine.rs:3928-3930`, but I found no scoped test asserting that a NIC-BAD IPv4 packet increments `ip.rx_csum_bad` exactly once. This is why the later offload wrapper drifted into double-counting the A2 counter.

- **FYI — default-build pure module tests cover the A2 parsers/builders; engine wiring is still mostly integration/test-inject territory.** This repeats the risk shape without re-flagging Claude's TAP-gated integration finding: `l2_decode`, `ip_decode`, `arp_decode`/builders, and `icmp_input` have local tests (`crates/dpdk-net-core/src/l2.rs:57-118`, `crates/dpdk-net-core/src/l3_ip.rs:233-438`, `crates/dpdk-net-core/src/arp.rs:362-756`, `crates/dpdk-net-core/src/icmp.rs:95-203`). The counter-placement bug above needs a small offload-aware engine-level test, not more parser tests.

## Observability gaps

- **BUG — bad-checksum observability is inflated on one path.** Same root cause as the invariant finding: NIC-BAD IP checksum classification bumps `ip.rx_csum_bad` in the offload wrapper at `crates/dpdk-net-core/src/l3_ip.rs:214-219`, then the generic engine drop arm bumps it again at `crates/dpdk-net-core/src/engine.rs:3928-3930`. Operators comparing NIC-BAD counter deltas with software-bad counter deltas will see a 2x IP-counter rate only when RX checksum offload is active and the NIC reports BAD.

- **FYI — null mbuf slots from `rx_burst` are skipped after `eth.rx_pkts` is already batch-added.** `poll_once` adds the full returned `n` to `eth.rx_pkts` at `crates/dpdk-net-core/src/engine.rs:2609-2610`, then skips null pointers defensively at `crates/dpdk-net-core/src/engine.rs:2629-2643`. DPDK promises populated slots are non-null, so this should be unreachable for real PMDs. If a misbehaving PMD or shim returns null inside the first `n` slots, the counter records a packet that was not decoded or freed. I classify this FYI because the skip is explicitly a panic-firewall defense for an invalid PMD state, not a normal-path counter contract.

## Memory-ordering / ARM-portability concerns

- None newly observed beyond the skip-listed `clock.rs` x86-only compile blocker and the implicit single-lcore `Cell`/`RefCell` contract called out by Claude.

- **FYI — A1/A2 counters use `fetch_add(..., Ordering::Relaxed)` consistently.** The helper contract is in `crates/dpdk-net-core/src/counters.rs:3-5`, with increments implemented through atomic RMW helpers later in the same module. The C ABI mirror documents helper-based atomic loads at `crates/dpdk-net/src/api.rs:299-309`. I did not find a mechanical Relaxed-vs-Acquire/Release bug in the scoped A1/A2 paths.

## C-ABI / FFI

- None newly observed beyond the skip-listed C ABI dead fields and `dpdk_net_eal_init` error-loss/panic findings.

- **FYI — A2 C ABI field order is stable at HEAD for the scoped L2/L3 additions.** The A2 fields remain in `dpdk_net_engine_config_t` as `local_ip`, `gateway_ip`, `gateway_mac`, and `garp_interval_sec` at `crates/dpdk-net/src/api.rs:46-50`, and `dpdk_net_engine_create` threads them into `EngineConfig` at `crates/dpdk-net/src/lib.rs:220-228`. I did not find a new A2-specific cbindgen omission in the generated production whitelist (`crates/dpdk-net/cbindgen.toml:54-74`), aside from Claude's broader whitelist-maintenance hazard.

## Hidden coupling

- **LIKELY-BUG — gratuitous-ARP and gateway-probe timers update their last-send timestamp even when the frame was not transmitted.** `maybe_emit_gratuitous_arp` attempts `tx_frame`, conditionally bumps `eth.tx_arp`, but then writes `*last = now` unconditionally at `crates/dpdk-net-core/src/engine.rs:6248-6254`. The post-A2 gateway probe path introduced by commit `7cbc2f6` has the same pattern: it conditionally bumps `tx_arp` only on successful `tx_frame`, but always sets `last_gw_arp_req_ns` at `crates/dpdk-net-core/src/engine.rs:6525-6537`. `tx_frame` returns false on allocation failure or full TX ring and records those failures at `crates/dpdk-net-core/src/engine.rs:2124-2140` and `crates/dpdk-net-core/src/engine.rs:2175-2183`. A transient no-mem/full-ring event therefore suppresses the next GARP/probe for a full interval even though nothing went onto the wire. For GARP this weakens refresh cadence; for zero-gateway-MAC probing it can delay discovery by one second per failed attempt.

- **SMELL — L2 broadcast acceptance is broader than the ARP-only comment says.** `l2_decode` documents "Accepts broadcast ... for ARP" at `crates/dpdk-net-core/src/l2.rs:25-27`, but the code accepts broadcast before checking ethertype at `crates/dpdk-net-core/src/l2.rs:38-47`, so broadcast IPv4 proceeds into L3. With a nonzero configured local IP, normal IPv4 limited-broadcast destination addresses still drop later as `NotOurs`, but an IPv4 frame with broadcast Ethernet destination and unicast IP destination equal to us will be accepted. This is low-risk, but it is hidden L2/L3 policy coupling that the comment does not make obvious.

## Documentation drift

- **SMELL — ICMP parser comment claims it requires the embedded transport 8 bytes, but code only requires the embedded IPv4 header.** The comment says "We need at least 20 bytes of IPv4 header + 8 bytes of original transport" at `crates/dpdk-net-core/src/icmp.rs:70-72`, but the actual guard is `ip_payload.len() < 8 + 20` at `crates/dpdk-net-core/src/icmp.rs:72-74`, and the implementation only reads the inner IPv4 header destination at `crates/dpdk-net-core/src/icmp.rs:75-81`. This is not a runtime bug because the transport bytes are unused, but the comment can mislead a future reviewer into thinking RFC 792's original-data requirement is enforced here.

- **FYI — the A2 review paths used `resd-*`; HEAD uses `dpdk-*`.** The skip-listed A2 reviews cite `crates/resd-net-core/...` and `crates/resd-net/...`, while HEAD files are under `crates/dpdk-net-core/...` and `crates/dpdk-net/...`. The code move itself is not a defect; this report anchors all findings to HEAD paths.

## FYI / informational

- **FYI — ARP frame padding fix remains present at HEAD.** The A2 mTCP review's E-1 was fixed by commit `eb4bbc8`; at HEAD `ARP_FRAME_LEN` is `14 + 28 + 18` at `crates/dpdk-net-core/src/arp.rs:15-20`, and all three builders zero the pad range at `crates/dpdk-net-core/src/arp.rs:166-168`, `crates/dpdk-net-core/src/arp.rs:195-197`, and `crates/dpdk-net-core/src/arp.rs:217-219`.

- **FYI — A1/A2 mbuf error paths reviewed here did not show a new leak/double-free.** `tx_frame` frees the allocated mbuf on append failure and TX-ring rejection (`crates/dpdk-net-core/src/engine.rs:2136-2140`, `crates/dpdk-net-core/src/engine.rs:2179-2183`), `dispatch_one_real_mbuf` frees each RX mbuf after decode (`crates/dpdk-net-core/src/engine.rs:3790-3792`), and `Engine::drop` clears flow-table mbuf owners before mempools drop (`crates/dpdk-net-core/src/engine.rs:6556-6574`). Later TCP retransmit ownership is outside the requested A1/A2 focus except where it touches the shared TX batch ring.

- **FYI — lock acquisition ordering in the scoped A1/A2 path looked mechanically simple.** EAL init uses one `Mutex<bool>` at `crates/dpdk-net-core/src/engine.rs:929-933`; the panic-on-poison issue is skip-listed via Claude. The A2 RX path uses `RefCell` borrows, not OS locks, and I did not find an A1/A2 deadlock cycle.

## Verification trace

Files read (all paths under `/home/ubuntu/resd.dpdk_tcp/.claude/worktrees/cross-phase-retro-review/` unless noted):
- `docs/superpowers/reviews/phase-a2-mtcp-compare.md:1-111` (full skip-list review)
- `docs/superpowers/reviews/phase-a2-rfc-compliance.md:1-119` (full skip-list review)
- `docs/superpowers/reviews/cross-phase-retro-part-1-claude.md:1-165` (full skip-list review; read before writing this report)
- `crates/dpdk-net-sys/Cargo.toml:1-14`
- `crates/dpdk-net-sys/build.rs:1-218`
- `crates/dpdk-net-sys/src/lib.rs:1-65`
- `crates/dpdk-net-sys/wrapper.h:1-115`
- `crates/dpdk-net-sys/shim.c:1-266`
- `crates/dpdk-net-core/src/lib.rs:1-78`
- `crates/dpdk-net-core/src/l2.rs:1-118`
- `crates/dpdk-net-core/src/l3_ip.rs:1-438`
- `crates/dpdk-net-core/src/arp.rs:1-756`
- `crates/dpdk-net-core/src/icmp.rs:1-203`
- `crates/dpdk-net-core/src/flow_table.rs:1-417`
- `crates/dpdk-net-core/src/clock.rs:1-150`
- `crates/dpdk-net-core/src/counters.rs:1-220` plus targeted grep hits through counter-name tables/tests
- `crates/dpdk-net-core/src/mempool.rs:1-343`
- `crates/dpdk-net-core/src/error.rs:1-81`
- `crates/dpdk-net-core/src/engine.rs:350-465,520-630,920-1010,1100-1568,1578-1855,2100-2350,2490-2830,3690-4010,5320-5520,5880-6225,6230-6630` plus targeted `rg` over EAL, poll, RX/TX, counters, unsafe, locks, and ARP timer helpers
- `crates/dpdk-net/src/api.rs:1-541`
- `crates/dpdk-net/src/lib.rs:1-1863` (tool output truncated in display, but targeted sections read around EAL, create, config pass-through, tests, and C ABI helpers)
- `crates/dpdk-net/build.rs:1-43`
- `crates/dpdk-net/cbindgen.toml:1-77`
- `crates/dpdk-net/cbindgen-test.toml:1-110`
- `crates/dpdk-net/Cargo.toml:1-40`
- `crates/dpdk-net/src/test_ffi.rs:1-349`
- `crates/dpdk-net/src/test_only.rs:1-33`
- `crates/dpdk-net/tests/api_shutdown.rs:1-105`
- `crates/dpdk-net/tests/panic_firewall.rs:1-31`
- `crates/dpdk-net/tests/test_header_excluded.rs:1-40`
- `include/dpdk_net.h` targeted grep hits for A2 counter/config fields
- `include/dpdk_net_counters_load.h` targeted grep hit for counter-load helper

Git commands run:
- `git status --short`
- `git log --oneline phase-a1-complete`
- `git log --oneline phase-a1-complete..phase-a2-complete`
- `git show --stat --oneline --decorate --find-renames 3476705 8aaeb9b 4d447b9 2de5b43 c069421 31dd6d3 f2b7910 12b12e4 4d4cb7b 4a886cc 19cfd68 ab68852 3666f5e 7a92d35 6b71c82 d3665db d738799 2d9398f 668b568 ce8ce71 d371ced 1d8a512 a50c02c 583585a 551bc3a 56ffd3f`
- `git show --stat --oneline --decorate --find-renames a10031e 5061a5a eb4bbc8 54d3573 19381c8 ab87ca9 1fb5143 ea3a457 3c0c99c 0419835 7b61b5e 10b50d0 5ae7849 42e5176 fd2486e 8925d57 d8dad1f b4c0041 878b530 f0aa33b f14e5d8 3534687 fba3589 7c7217b`
- `git show --patch --find-renames --stat 3476705 8aaeb9b 4d447b9 2de5b43 c069421 31dd6d3 f2b7910 12b12e4 4d4cb7b 4a886cc 19cfd68 ab68852 3666f5e 7a92d35 6b71c82 d3665db d738799 2d9398f -- crates/resd-net-sys crates/resd-net-core crates/resd-net crates/dpdk-net-sys crates/dpdk-net-core crates/dpdk-net include scripts tests examples`
- `git show --patch --find-renames --stat 668b568 ce8ce71 d371ced 1d8a512 a50c02c 583585a 551bc3a 56ffd3f -- docs/superpowers/specs docs/superpowers/plans`
- `git show --patch --find-renames --stat a10031e 5061a5a eb4bbc8 54d3573 19381c8 ab87ca9 1fb5143 ea3a457 3c0c99c 0419835 7b61b5e 10b50d0 5ae7849 42e5176 fd2486e 8925d57 d8dad1f b4c0041 878b530 f0aa33b f14e5d8 3534687 fba3589 7c7217b -- crates/resd-net-sys crates/resd-net-core crates/resd-net crates/dpdk-net-sys crates/dpdk-net-core crates/dpdk-net include scripts tests examples docs/superpowers/reviews docs/superpowers/plans docs/superpowers/specs`
- `git log --oneline -S 'ip_decode_offload_aware' -- crates/dpdk-net-core/src/l3_ip.rs crates/resd-net-core/src/l3_ip.rs`
- `git log --oneline -S 'maybe_probe_gateway_mac' -- crates/dpdk-net-core/src/engine.rs crates/resd-net-core/src/engine.rs`
- `git log --oneline -S 'mbuf_data_room + sys::RTE_PKTMBUF_HEADROOM' -- crates/dpdk-net-core/src/engine.rs crates/resd-net-core/src/engine.rs`
- `git log --oneline -S 'is_broadcast' -- crates/dpdk-net-core/src/l2.rs crates/resd-net-core/src/l2.rs`

Mechanical-defect category coverage:
- Arithmetic edges: finding on `mbuf_data_room + RTE_PKTMBUF_HEADROOM`; no other A1/A2 arithmetic overflow found in the pure parsers.
- Atomic / memory ordering: no new A1/A2 bug observed; Relaxed counters are documented and consistently atomic.
- Lock acquisition ordering: no new deadlock cycle observed; only A1 `EAL_INIT` mutex issue is skip-listed for panic handling.
- Mempool / mbuf leak edges: no new A1/A2 leak/double-free observed in scoped TX/RX/Drop paths.
- Unsafe-block invariants: no new A1/A2 raw-pointer invariant violation observed beyond the arithmetic-driven DPDK data-room issue and skip-listed FFI panic issues.
- Error-path correctness: findings on ARP timer last-send state after failed `tx_frame` and TX ring capacity edge.
- Timer ordering: finding on GARP/probe retry timestamp update after failed TX.
- Counter increment placement: finding on duplicate `ip.rx_csum_bad` increment; FYI on null-slot `rx_pkts` overcount under invalid PMD output.
