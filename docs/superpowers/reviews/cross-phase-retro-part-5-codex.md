# Part 5 Cross-Phase Retro Review (Codex)
**Reviewer:** codex:codex-rescue
**Reviewed at:** 2026-05-05
**Part:** 5 — ENA HW offloads (TX/RX cksum, RSS, mbuf fast-free, RX timestamp, runtime knobs)
**Phases:** A-HW, A-HW+

## Verdict

- BUG: NIC-reported IPv4 checksum BAD is double-counted in `ip.rx_csum_bad`. The offload-aware wrapper handles `CksumOutcome::Bad` by incrementing `eth.rx_drop_cksum_bad` and `ip.rx_csum_bad`, then returns `Err(L3Drop::CsumBad)` at `crates/dpdk-net-core/src/l3_ip.rs:211`-`220`; the caller's generic `L3Drop::CsumBad` arm increments `ip.rx_csum_bad` again at `crates/dpdk-net-core/src/engine.rs:3928`-`3930`. The software checksum-bad path should still increment once via `engine.rs`, but the NIC-BAD path already consumed the IP-specific bad checksum event in `l3_ip.rs`. This matches the suspected Part-1 counter-placement issue: the origin is the A-HW offload wrapper returning the same drop enum that the software path uses, after already bumping the same IP counter.

- FYI: No atomic runtime toggle exists at HEAD for `tx_cksum_offload_active` / `rx_cksum_offload_active`; they are plain per-engine booleans initialized from `PortConfigOutcome` and stored on `Engine` at `crates/dpdk-net-core/src/engine.rs:774`-`790` and `crates/dpdk-net-core/src/engine.rs:1529`-`1531`. I therefore found no Acquire/Release issue in the requested runtime-toggle sense. The counter atomics in this surface use `Relaxed`, which is consistent with observability-only counters and not used to publish offload state.

## Architectural drift

- FYI: The scoped filenames named in the prompt no longer map 1:1 to HEAD. `crates/dpdk-net-core/src/eal.rs`, `crates/dpdk-net-core/src/offloads.rs`, and `crates/dpdk-net-core/src/l2_eth.rs` are absent; live A-HW/A-HW+ behavior is embedded in `engine.rs`, `tcp_output.rs`, `l3_ip.rs`, `flow_table.rs`, `ena_xstats.rs`, `wc_verify.rs`, and `llq_verify.rs`. The module exports at `crates/dpdk-net-core/src/lib.rs:17`-`22` confirm the live names are `l2`, `l3_ip`, `llq_verify`, `wc_verify`, and `ena_xstats`, not the older file names.

- SMELL: The A-HW feature contract in `Cargo.toml` says feature-off builds compile offload code paths away while struct fields stay present for C ABI stability (`crates/dpdk-net-core/Cargo.toml:31`-`34`), but by HEAD the implementation is not isolated in an `offloads.rs` module. The mechanical risk is not behavior today; it is reviewability. TX checksum metadata is in `tcp_output.rs`, RX checksum classification is split between `l3_ip.rs` and `engine.rs`, RSS hash selection is in `flow_table.rs`, and negotiated capability latches are in `engine.rs`.

## Cross-phase invariant violations

- BUG: `ip.rx_csum_bad` violates the one-drop-one-counter-event invariant on NIC-reported IPv4 checksum BAD. The offload branch increments `ip.rx_csum_bad` at `crates/dpdk-net-core/src/l3_ip.rs:218`, then returns `Err(L3Drop::CsumBad)` at `crates/dpdk-net-core/src/l3_ip.rs:219`; `handle_ipv4` unconditionally handles that enum by incrementing the same counter at `crates/dpdk-net-core/src/engine.rs:3928`-`3930`. By contrast, software checksum failure enters only through `ip_decode(.., nic_csum_ok=false)` and should be counted at the caller. The fix shape should be to move the IP counter bump to exactly one layer, or return a distinct NIC-BAD drop kind that the caller does not count as a software checksum failure.

- FYI: The L4 counterpart does not show the same double-count pattern. NIC-reported L4 checksum BAD increments `eth.rx_drop_cksum_bad` and `tcp.rx_bad_csum` then returns before `parse_segment` runs at `crates/dpdk-net-core/src/engine.rs:4034`-`4048`. Software L4 checksum failure increments `tcp.rx_bad_csum` only in the `TcpParseError::Csum` arm at `crates/dpdk-net-core/src/engine.rs:4060`-`4073`.

## Tech debt accumulated

- SMELL: `tx_offload_rewrite_cksums` computes `pseudo_len` with `wrapping_add` at `crates/dpdk-net-core/src/tcp_output.rs:270`, then calls `tcp_pseudo_header_checksum`, whose only bound check is a debug assertion at `crates/dpdk-net-core/src/tcp_output.rs:223`-`234`. Current callers are protected because `build_segment_inner` rejects IPv4 total lengths above `u16::MAX` at `crates/dpdk-net-core/src/tcp_output.rs:115`-`118`, and retransmit payload length is already `u16`-bounded in current state. Still, the helper is public within the crate and accepts arbitrary `tcp_hdr_len`/`payload_for_csum_len`; in release builds an oversized direct caller would silently truncate the pseudo-header length field. This is not an observed production bug at HEAD, but it is an arithmetic edge worth hardening with a non-debug bound check.

- SMELL: RX timestamp dynflag mask construction uses `Some(1u64 << flag_bit)` after checking only `flag_bit >= 0` at `crates/dpdk-net-core/src/engine.rs:1396`-`1404`. DPDK dynflag bit positions are expected to be in range, and ENA steady state returns `None`, but the Rust expression can panic in debug or produce invalid behavior if a buggy shim/PMD returns `flag_bit >= 64`. The adjacent dynfield offset path is guarded by `off_rc >= 0` and then read through the shim at `crates/dpdk-net-core/src/engine.rs:1883`-`1894`.

## Test-pyramid concerns

- SMELL: The counter-coverage test for `eth.rx_drop_cksum_bad` is a one-shot synthetic bump, not an execution of the offload classifier path. The test documents that the real path requires an all-features build and static audit at `crates/dpdk-net-core/tests/counter-coverage.rs:377`-`388`. That means the confirmed `ip.rx_csum_bad` double-bump can pass this coverage layer because no test asserts the combined `eth.rx_drop_cksum_bad == 1` and `ip.rx_csum_bad == 1` behavior for a single NIC-reported IP checksum BAD frame.

- SMELL: The software IP checksum-bad coverage is separate and only asserts `ip.rx_csum_bad > 0` after injecting a corrupt IPv4 header at `crates/dpdk-net-core/tests/counter-coverage.rs:522`-`532`. It does not distinguish software-bad from NIC-bad accounting. A focused pure unit around `ip_decode_offload_aware` with `RTE_MBUF_F_RX_IP_CKSUM_BAD` plus caller-level handling would catch the current double increment.

- FYI: Feature-off knob tests do cover that RX checksum offload is ignored when compiled out: `ip_decode_offload_aware` is called with `ol_flags = u64::MAX` and `rx_cksum_offload_active = true`, yet the test expects software verification and no `eth.rx_drop_cksum_bad` bump at `crates/dpdk-net-core/tests/knob-coverage.rs:755`-`790`. This is good offload-off coverage, but it does not exercise the default feature-on NIC-BAD path.

## Observability gaps

- BUG: The double increment makes `ip.rx_csum_bad` misleading relative to `eth.rx_drop_cksum_bad`. For one NIC-reported bad IPv4 checksum, `eth.rx_drop_cksum_bad` increments once at `crates/dpdk-net-core/src/l3_ip.rs:214`-`217`, while `ip.rx_csum_bad` increments once there and once again in `handle_ipv4` at `crates/dpdk-net-core/src/engine.rs:3928`-`3930`. Operators comparing L2/eth offload-bad drops with L3 checksum-bad drops will see a false 2:1 ratio for this path.

- FYI: ENA xstat failure handling preserves cumulative counters on scrape error and only zeros the allowance snapshot counters. That behavior is explicit in `apply_on_error` at `crates/dpdk-net-core/src/ena_xstats.rs:96`-`128` and dispatched by `apply_scrape_result` at `crates/dpdk-net-core/src/ena_xstats.rs:138`-`148`. I did not find an xstats reset-on-error mechanical defect in this HEAD.

## Memory-ordering / ARM-portability concerns

- FYI: The offload-active fields are not atomics. They are immutable after engine construction for the lifetime of an `Engine`: field declarations are plain `bool` at `crates/dpdk-net-core/src/engine.rs:784` and `crates/dpdk-net-core/src/engine.rs:790`, and assignments are from the local `outcome` at `crates/dpdk-net-core/src/engine.rs:1529`-`1531`. Since no concurrent writer exists in the inspected code, there is no Acquire/Release publication bug to classify.

- FYI: Offload counters use relaxed atomics, e.g. unsupported offload bits increment via `fetch_add(..., Relaxed)` at `crates/dpdk-net-core/src/engine.rs:1089`-`1099`, checksum-bad counters do the same at `crates/dpdk-net-core/src/l3_ip.rs:214`-`218` and `crates/dpdk-net-core/src/engine.rs:4038`-`4047`, and xstat snapshots use relaxed stores at `crates/dpdk-net-core/src/ena_xstats.rs:73`-`93`. I did not find a correctness dependency on observing these counters in a synchronizing order.

## C-ABI / FFI

- SMELL: RX timestamp dynfield arithmetic trusts the dynamic flag bit without a range check, as noted above. The offset itself is not hand-added in Rust; it is passed to `shim_rte_mbuf_read_dynfield_u64` after coming from `rte_mbuf_dynfield_lookup` at `crates/dpdk-net-core/src/engine.rs:1390`-`1403` and `crates/dpdk-net-core/src/engine.rs:1883`-`1894`. The FFI boundary would be stronger if `flag_bit` used `checked_shl` or an explicit `< 64` guard.

- FYI: The opaque engine allocation and destroy path has matching `Box::into_raw(Box::new(OpaqueEngine(e)))` and `Box::from_raw(p as *mut OpaqueEngine)` at `crates/dpdk-net/src/lib.rs:51`-`56` and `crates/dpdk-net/src/lib.rs:272`-`279`. I did not find a Box layout mismatch in the inspected C-ABI path.

- FYI: `dpdk_net_scrape_xstats` obtains an immutable engine reference and calls the slow-path scraper at `crates/dpdk-net/src/lib.rs:661`-`668`; no mutable alias is created there. Test-only mutable raw access is gated under `test-server` at `crates/dpdk-net/src/lib.rs:65`-`76`, outside the production default path.

## Hidden coupling

- SMELL: The offload init path creates half-initialized PMD state before some later fallible checks. `Engine::new` calls `rte_eth_dev_configure` inside `configure_port_offloads`, then queue setup and `rte_eth_dev_start`, and only after start runs LLQ verification, RSS RETA programming, RX timestamp dynfield lookup, MAC lookup, and xstat map resolution at `crates/dpdk-net-core/src/engine.rs:1313`-`1440`. If `verify_llq_activation_from_global` returns an error at `crates/dpdk-net-core/src/engine.rs:1362`-`1367`, `Engine` is not constructed, so `Engine::drop` will not run the `rte_eth_dev_stop` / `rte_eth_dev_close` cleanup at `crates/dpdk-net-core/src/engine.rs:6605`-`6609`. I did not prove this leaks mbufs, because the Rust mempools are still local values and will drop, but the DPDK port may remain configured/started after this early return.

- SMELL: The same cleanup concern applies to errors after `rte_eth_dev_start`, including `program_rss_reta_single_queue(...) ?` at `crates/dpdk-net-core/src/engine.rs:1371`-`1375` and MAC lookup failure at `crates/dpdk-net-core/src/engine.rs:1427`-`1433`. A local port-start guard would make the cleanup invariant mechanical instead of relying on all post-start steps being infallible.

- FYI: RefCell borrow ordering around TX ring drain is explicitly scoped. `send_bytes` drops the `tx_pending_data.borrow_mut()` before calling `drain_tx_pending_data`, then re-borrows afterward at `crates/dpdk-net-core/src/engine.rs:5474`-`5495`; retransmit follows the same pattern at `crates/dpdk-net-core/src/engine.rs:6191`-`6208`. I did not find a nested `borrow_mut` panic in these offload-adjacent paths.

## Documentation drift

- FYI: Comments in `Cargo.toml` still describe `hw-offload-rss-hash` as "consume it when present" for future RSS-aware paths at `crates/dpdk-net-core/Cargo.toml:48`-`50`, while current `FlowTable::lookup_by_hash` explicitly ignores the computed `bucket_hash` because the backing table is still a `HashMap` at `crates/dpdk-net-core/src/flow_table.rs:157`-`176`. The live behavior is mechanically safe because lookup still keys by full tuple, but the feature's practical benefit is mostly deferred.

- SMELL: Some counter-coverage comments have stale approximate line references, e.g. `engine.rs ~1011` for RX timestamp at `crates/dpdk-net-core/tests/counter-coverage.rs:368`-`374`, while the live timestamp miss bump is at `crates/dpdk-net-core/src/engine.rs:1404`-`1408`. This is not a runtime defect, but it weakens future mechanical audits.

## FYI / informational

- FYI: Internet checksum complement arithmetic is implemented as a streaming fold with carry-over odd bytes across chunks and final one's-complement fold at `crates/dpdk-net-core/src/l3_ip.rs:37`-`68`. The unit coverage exercises folding and classifier mappings at `crates/dpdk-net-core/src/l3_ip.rs:266`-`276` and `crates/dpdk-net-core/src/l3_ip.rs:387`-`437`. I did not find an arithmetic defect in the checksum fold itself.

- FYI: TX software checksum and offload pseudo-header setup are internally consistent for current callers. `build_segment_inner` rejects IPv4/TCP lengths above the 16-bit total-length limit at `crates/dpdk-net-core/src/tcp_output.rs:115`-`118`; full TCP checksum uses pseudo-header plus TCP header plus payload at `crates/dpdk-net-core/src/tcp_output.rs:164`-`177`; offload rewrite writes pseudo-header-only TCP checksum and zeros IPv4 checksum at `crates/dpdk-net-core/src/tcp_output.rs:259`-`278`. ENA driver expectations are represented by `RTE_MBUF_F_TX_IPV4 | RTE_MBUF_F_TX_IP_CKSUM | RTE_MBUF_F_TX_TCP_CKSUM` and `l2/l3/l4_len` setup at `crates/dpdk-net-core/src/tcp_output.rs:320`-`340`.

- FYI: RSS hash truncation/cast is not currently a correctness issue because `nic_rss_hash` is already `u32` from the shim and `lookup_by_hash` ignores the supplied bucket hash under the current `HashMap` backing store at `crates/dpdk-net-core/src/flow_table.rs:157`-`176`. The software fallback truncates SipHash to `u32` at `crates/dpdk-net-core/src/flow_table.rs:43`-`61`, but that value is also only informational today.

- FYI: MBUF_FAST_FREE is requested only as a PMD offload bit when the feature is enabled at `crates/dpdk-net-core/src/engine.rs:1720`-`1731`. I did not find a Rust-side free-path bypass introduced by the feature. Mbuf release still routes through `shim_rte_pktmbuf_free` / `shim_rte_pktmbuf_free_seg` in the inspected TX/RX paths, including the A10 cliff fix in `MbufHandle::Drop` at `crates/dpdk-net-core/src/mempool.rs:261`-`312`.

- FYI: I found no new A-HW timer-wheel entry for RX timestamp epoch wrap. RX timestamp is read as an optional per-mbuf value at `crates/dpdk-net-core/src/engine.rs:1865`-`1900` and threaded through RX dispatch at `crates/dpdk-net-core/src/engine.rs:3776`-`3784`; timer-wheel construction and use remain unrelated to HW timestamp wrap in the inspected ranges.

## Verification trace

Git / scope commands run:

- `git log --oneline phase-a6-6-7-complete..phase-a-hw-complete` — returned no commits at this HEAD.
- `git log --oneline phase-a-hw-complete..phase-a-hw-plus-complete` — returned A-HW+ commits plus older history due to tag topology.
- `git rev-parse phase-a6-6-7-complete phase-a-hw-complete phase-a-hw-plus-complete HEAD`.
- `git merge-base --is-ancestor phase-a6-6-7-complete phase-a-hw-complete; printf '%s\n' $?` — returned `1`, so the requested A-HW range is not an ancestor-shaped range in this worktree.
- `git log --oneline --decorate --max-count=40 HEAD`.
- `git tag --list 'phase-a*' --sort=creatordate`.
- `git log --oneline --decorate --all --ancestry-path phase-a6-6-7-complete..phase-a-hw-complete`.
- `git log --oneline --decorate --all --ancestry-path phase-a-hw-complete..phase-a-hw-plus-complete`.
- `git diff --name-status phase-a6-6-7-complete..phase-a-hw-complete -- crates/dpdk-net-core/src crates/dpdk-net-core/Cargo.toml crates/dpdk-net-core/tests tests`.
- `git diff --name-status phase-a-hw-complete..phase-a-hw-plus-complete -- crates/dpdk-net-core/src crates/dpdk-net-core/Cargo.toml crates/dpdk-net-core/tests tests`.
- `git log --oneline --ancestry-path ... -- crates/dpdk-net-core/src crates/dpdk-net-core/Cargo.toml crates/dpdk-net-core/tests tests`.
- `git status --short`.
- `rg --files crates/dpdk-net-core/src | rg '(^|/)(eal|offloads|l3_ip|l2_eth|l2)\.rs$'` — confirmed `l3_ip.rs` and `l2.rs`; no `eal.rs`, `offloads.rs`, or `l2_eth.rs`.
- Targeted `rg -n` scans for offload/checksum/RSS/dynfield/timestamp/unsafe/counter patterns across scoped source and tests.

Files read with line ranges:

- `crates/dpdk-net-core/Cargo.toml`: 1-114.
- `crates/dpdk-net-core/src/lib.rs`: 1-100.
- `crates/dpdk-net-core/src/l2.rs`: 1-118.
- `crates/dpdk-net-core/src/l3_ip.rs`: 1-438.
- `crates/dpdk-net-core/src/engine.rs`: 760-860, 1040-1850, 1848-2360, 2500-2830, 3680-4105, 5400-5505, 5840-6225, 6528-6620.
- `crates/dpdk-net-core/src/tcp_output.rs`: 1-675.
- `crates/dpdk-net-core/src/flow_table.rs`: 1-260.
- `crates/dpdk-net-core/src/ena_xstats.rs`: 1-340.
- `crates/dpdk-net-core/src/wc_verify.rs`: 1-150.
- `crates/dpdk-net-core/src/llq_verify.rs`: 220-320.
- `crates/dpdk-net-core/src/mempool.rs`: 1-360.
- `crates/dpdk-net-core/src/tcp_input.rs`: 1120-1245.
- `crates/dpdk-net-core/src/tcp_reassembly.rs`: 360-430.
- `crates/dpdk-net/src/lib.rs`: 40-125, 141-285, 650-730.
- `crates/dpdk-net-core/tests/ahw_smoke.rs`: 1-260.
- `crates/dpdk-net-core/tests/ahw_smoke_ena_hw.rs`: 1-320.
- `crates/dpdk-net-core/tests/ena_obs_smoke.rs`: 1-61.
- `crates/dpdk-net-core/tests/counter-coverage.rs`: 360-392, 520-538, 1428-1442.
- `crates/dpdk-net-core/tests/knob-coverage.rs`: 736-825.

Skipped per instruction:

- Did not re-review `phase-a-hw-{mtcp,rfc}.md`, `phase-a-hw-plus-{mtcp,rfc}.md`, or prior `cross-phase-retro-part-{1,2,3,4}-{claude,codex,synthesis}.md` reports.
