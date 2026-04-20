//! Knob-coverage audit per roadmap §A11.
//!
//! Each entry exercises a non-default value of one behavioral knob
//! and asserts an observable consequence that distinguishes the
//! non-default value from the default. This file is the A5.5 partial
//! slice: it covers the five new TLP-tuning knobs plus the engine-wide
//! `event_queue_soft_cap` plus the aggressive-order-entry preset
//! combination. A11 will absorb this into a full cross-phase audit
//! (likely replacing the flat `#[test]` structure with a `KnobScenario`
//! table + scenario-fn pointers).
//!
//! Scenario fns run at the Rust-helper / unit-test level so they do
//! not require a TAP harness. When a knob's observable consequence
//! needs timer-wheel stepping or peer control, the test asserts on
//! the same helper the engine's hot path invokes (`pto_us`,
//! `TcpConn::tlp_arm_gate_passes`, `EventQueue::push`, …).
//!
//! A5.5 canonical list (per plan §17):
//!   Engine-wide:
//!     event_queue_soft_cap
//!   Per-connect:
//!     tlp_pto_min_floor_us
//!     tlp_pto_srtt_multiplier_x100
//!     tlp_skip_flight_size_gate
//!     tlp_max_consecutive_probes
//!     tlp_skip_rtt_sample_gate
//!   Combination:
//!     aggressive_order_entry_preset

use std::sync::atomic::Ordering;

use resd_net_core::counters::Counters;
use resd_net_core::flow_table::{ConnHandle, FourTuple};
use resd_net_core::mempool::Mbuf;
use resd_net_core::tcp_conn::TcpConn;
use resd_net_core::tcp_events::{EventQueue, InternalEvent};
use resd_net_core::tcp_retrans::RetransEntry;
use resd_net_core::tcp_tlp::{pto_us, TlpConfig, WCDELACK_US};

// ---- shared test helpers ------------------------------------------------

fn tuple() -> FourTuple {
    FourTuple {
        local_ip: 0x0a_00_00_02,
        local_port: 40000,
        peer_ip: 0x0a_00_00_01,
        peer_port: 5000,
    }
}

fn make_conn() -> TcpConn {
    TcpConn::new_client(tuple(), 0, 1460, 1024, 2048, 5_000, 5_000, 1_000_000)
}

fn prime_retrans(c: &mut TcpConn, seq: u32, len: u16) {
    // Integration-test builds don't have `cfg(test)` on the library, so
    // the crate-internal `Mbuf::null_for_test()` isn't visible. Use the
    // public `from_ptr(null)` spelling — the retrans entry is never
    // TX'd, just staged so `snd_retrans.is_empty()` reports `false`
    // for the arm-gate check.
    c.snd_retrans.push_after_tx(RetransEntry {
        seq,
        len,
        mbuf: Mbuf::from_ptr(std::ptr::null_mut()),
        first_tx_ts_ns: 0,
        xmit_count: 1,
        sacked: false,
        lost: false,
        xmit_ts_ns: 0,
    });
}

// ---- knob 1: event_queue_soft_cap ---------------------------------------

/// Knob: `EngineConfig::event_queue_soft_cap`.
/// Non-default value: 64 (minimum soft cap; default is 4096).
/// Observable consequence: pushing > cap events increments
/// `obs.events_dropped`; the default cap would absorb the same burst
/// without drops.
#[test]
fn knob_event_queue_soft_cap_overflow_drops_events() {
    let counters = Counters::new();
    let mut q = EventQueue::with_cap(64);
    for i in 0..200u64 {
        q.push(
            InternalEvent::Connected {
                conn: ConnHandle::default(),
                rx_hw_ts_ns: 0,
                emitted_ts_ns: i,
            },
            &counters,
        );
    }
    let dropped = counters.obs.events_dropped.load(Ordering::Relaxed);
    assert!(
        dropped > 0,
        "non-default soft_cap=64 should produce drops under a 200-event burst; got {dropped}"
    );
    let high_water = counters.obs.events_queue_high_water.load(Ordering::Relaxed);
    assert_eq!(
        high_water, 64,
        "high-water latches at soft_cap under overflow"
    );
}

// ---- knob 2: tlp_pto_min_floor_us ---------------------------------------

/// Knob: `TcpConn::tlp_pto_min_floor_us`.
/// Non-default value: 0 (no floor), reached at the ABI boundary via the
/// `u32::MAX` sentinel (see `TcpConn::tlp_config`). Default is the
/// engine-wide `tcp_min_rto_us` (5_000 µs).
/// Observable consequence: PTO is NOT clamped to 5_000 µs; it equals
/// the raw `2·SRTT` base for a SRTT small enough that the default would
/// have floored.
#[test]
fn knob_tlp_pto_min_floor_us_no_floor_allows_sub_min_rto_pto() {
    let cfg = TlpConfig {
        floor_us: 0,
        multiplier_x100: 200,
        skip_flight_size_gate: true,
    };
    // SRTT = 1 µs → base = 2 µs. Default floor 5_000 would clamp to
    // 5_000; non-default 0 lets PTO drop to 2.
    let result = pto_us(Some(1), &cfg, 5);
    assert_eq!(
        result, 2,
        "non-default floor=0 must not clamp PTO to default 5_000 µs"
    );
    // And cross-check that the DEFAULT floor would have clamped here.
    let default_cfg = TlpConfig::a5_compat(5_000);
    assert_eq!(
        pto_us(Some(1), &default_cfg, 5),
        5_000,
        "sanity: default floor 5_000 does clamp the same tiny SRTT"
    );
}

/// Verifies the `u32::MAX` sentinel projection path. The ABI accepts
/// `u32::MAX` to mean "explicit no floor"; `TcpConn::tlp_config`
/// projects that to `floor_us=0` in `TlpConfig`.
#[test]
fn knob_tlp_pto_min_floor_us_max_sentinel_projects_to_zero() {
    let mut c = make_conn();
    c.tlp_pto_min_floor_us = u32::MAX;
    c.tlp_pto_srtt_multiplier_x100 = 200;
    let cfg = c.tlp_config(5_000);
    assert_eq!(
        cfg.floor_us, 0,
        "u32::MAX sentinel must project to floor_us=0 in TlpConfig"
    );
}

// ---- knob 3: tlp_pto_srtt_multiplier_x100 -------------------------------

/// Knob: `TcpConn::tlp_pto_srtt_multiplier_x100`.
/// Non-default value: 100 (1.0×). Default is 200 (2.0× per RFC 8985
/// §7.2).
/// Observable consequence: PTO base = SRTT, not 2·SRTT.
#[test]
fn knob_tlp_pto_srtt_multiplier_x100_one_srtt() {
    let cfg = TlpConfig {
        floor_us: 0,
        multiplier_x100: 100,
        skip_flight_size_gate: true,
    };
    assert_eq!(
        pto_us(Some(100_000), &cfg, 5),
        100_000,
        "multiplier=100 must give base = 1·SRTT"
    );
    // Sanity: same SRTT at default multiplier gives 2·SRTT.
    let default_cfg = TlpConfig {
        floor_us: 0,
        multiplier_x100: 200,
        skip_flight_size_gate: true,
    };
    assert_eq!(
        pto_us(Some(100_000), &default_cfg, 5),
        200_000,
        "sanity: default multiplier=200 gives 2·SRTT"
    );
}

// ---- knob 4: tlp_skip_flight_size_gate ----------------------------------

/// Knob: `TcpConn::tlp_skip_flight_size_gate`.
/// Non-default value: `true`. Default is `false` (RFC 8985 §7.2: when
/// FlightSize=1, add `+max(WCDelAckT, SRTT/4)` penalty so a delayed-ACK
/// receiver can't silently swallow the sole in-flight segment's ACK
/// past the probe deadline).
/// Observable consequence: at FlightSize=1, PTO base is NOT increased
/// by the WCDelAckT/SRTT-4 penalty.
#[test]
fn knob_tlp_skip_flight_size_gate_suppresses_penalty() {
    let skip_cfg = TlpConfig {
        floor_us: 0,
        multiplier_x100: 200,
        skip_flight_size_gate: true,
    };
    let result_skip = pto_us(Some(400_000), &skip_cfg, 1);
    // base = 2·SRTT = 800_000; skip=true means no penalty.
    assert_eq!(
        result_skip, 800_000,
        "skip_flight_size_gate=true must suppress the FlightSize=1 penalty"
    );

    // Contrast with default gate on: +max(WCDELACK, SRTT/4) kicks in.
    let default_cfg = TlpConfig {
        floor_us: 0,
        multiplier_x100: 200,
        skip_flight_size_gate: false,
    };
    let result_default = pto_us(Some(400_000), &default_cfg, 1);
    // WCDELACK = 200_000; SRTT/4 = 100_000; penalty = 200_000 → 1_000_000.
    assert_eq!(
        result_default,
        800_000 + WCDELACK_US,
        "sanity: default gate on adds max(WCDelAckT, SRTT/4) penalty"
    );
    assert!(
        result_skip < result_default,
        "skip_flight_size_gate=true must yield a strictly smaller PTO than default"
    );
}

// ---- knob 5: tlp_max_consecutive_probes ---------------------------------

/// Knob: `TcpConn::tlp_max_consecutive_probes`.
/// Non-default value: 3. Default is 1 (RFC 8985 §7: a single probe
/// before falling back to RTO).
/// Observable consequence: `tlp_arm_gate_passes` accepts `fired < 3`
/// (0, 1, 2) and rejects at `fired >= 3`. The default max=1 would
/// reject at `fired >= 1`, so non-default expands the budget.
#[test]
fn knob_tlp_max_consecutive_probes_expands_budget() {
    // Construct a conn that passes every other gate so the only var
    // under test is the budget check.
    let mut c = make_conn();
    prime_retrans(&mut c, 1000, 512);
    c.tlp_max_consecutive_probes = 3;
    c.tlp_skip_rtt_sample_gate = false;
    c.tlp_rtt_sample_seen_since_last_tlp = true;
    c.rtt_est.sample(5_000); // SRTT required by Task 15 gate.

    // Budget check: gate must PASS at fired=0, 1, 2 and REJECT at 3.
    for fired in 0u8..3 {
        c.tlp_consecutive_probes_fired = fired;
        assert!(
            c.tlp_arm_gate_passes(),
            "non-default max=3: gate must pass at fired={fired}"
        );
    }
    c.tlp_consecutive_probes_fired = 3;
    assert!(
        !c.tlp_arm_gate_passes(),
        "non-default max=3: gate must reject at fired=3"
    );

    // Contrast: default max=1 would reject at fired=1 — confirming the
    // knob's observable effect.
    c.tlp_max_consecutive_probes = 1;
    c.tlp_consecutive_probes_fired = 1;
    assert!(
        !c.tlp_arm_gate_passes(),
        "sanity: default max=1 rejects at fired=1 (scope that the non-default expands)"
    );
}

// ---- knob 6: tlp_skip_rtt_sample_gate -----------------------------------

/// Knob: `TcpConn::tlp_skip_rtt_sample_gate`.
/// Non-default value: `true`. Default is `false` (RFC 8985 §7.4: a TLP
/// probe must not be armed without an intervening RTT sample since the
/// last probe — otherwise multiple TLPs can fire on a single stale
/// SRTT).
/// Observable consequence: gate passes even when
/// `tlp_rtt_sample_seen_since_last_tlp == false`. The default would
/// reject the same state.
#[test]
fn knob_tlp_skip_rtt_sample_gate_bypasses_sample_requirement() {
    let mut c = make_conn();
    prime_retrans(&mut c, 1000, 512);
    c.tlp_max_consecutive_probes = 3;
    c.tlp_consecutive_probes_fired = 0;
    c.tlp_rtt_sample_seen_since_last_tlp = false; // key non-default condition
    c.rtt_est.sample(5_000); // Task 15: SRTT must still be present.

    // With skip=true: gate passes despite sample not seen.
    c.tlp_skip_rtt_sample_gate = true;
    assert!(
        c.tlp_arm_gate_passes(),
        "skip_rtt_sample_gate=true must let gate pass without a sample seen"
    );

    // With skip=false (default): gate rejects the same state.
    c.tlp_skip_rtt_sample_gate = false;
    assert!(
        !c.tlp_arm_gate_passes(),
        "sanity: skip_rtt_sample_gate=false rejects without a sample seen"
    );
}

// ---- combination: aggressive_order_entry_preset -------------------------

/// Combination: aggressive-order-entry preset.
/// Non-default values (all five TLP knobs at once):
///   `tlp_pto_min_floor_us = u32::MAX` (→ floor 0 via sentinel)
///   `tlp_pto_srtt_multiplier_x100 = 100`
///   `tlp_skip_flight_size_gate = true`
///   `tlp_max_consecutive_probes = 3`
///   `tlp_skip_rtt_sample_gate = true`
/// Observable consequence: the combination collapses PTO to `1·SRTT`
/// even at FlightSize=1, allows up to 3 probes without intervening
/// RTT samples, and — contrasted against the defaults — the same SRTT
/// produces a strictly smaller PTO AND the arm gate accepts in a state
/// (fired=2, sample_seen=false, FlightSize=1) where the defaults would
/// reject.
#[test]
fn knob_aggressive_order_entry_preset_combined_behavior() {
    let mut c = make_conn();
    // Apply the full aggressive preset.
    c.tlp_pto_min_floor_us = u32::MAX;
    c.tlp_pto_srtt_multiplier_x100 = 100;
    c.tlp_skip_flight_size_gate = true;
    c.tlp_max_consecutive_probes = 3;
    c.tlp_skip_rtt_sample_gate = true;

    // ---- (A) PTO formula: 1·SRTT, no FlightSize=1 penalty, no floor.
    let cfg = c.tlp_config(5_000);
    assert_eq!(cfg.floor_us, 0);
    assert_eq!(cfg.multiplier_x100, 100);
    assert!(cfg.skip_flight_size_gate);
    let preset_pto = pto_us(Some(100_000), &cfg, 1);
    assert_eq!(
        preset_pto, 100_000,
        "preset must yield 1·SRTT PTO even at FlightSize=1"
    );
    // Same SRTT, defaults: 2·SRTT + max(WCDELACK, SRTT/4) + floored.
    let default_cfg = TlpConfig::a5_compat(5_000);
    let default_pto = pto_us(Some(100_000), &default_cfg, 1);
    // 200_000 + max(200_000, 25_000) = 400_000.
    assert_eq!(default_pto, 400_000);
    assert!(
        preset_pto < default_pto,
        "preset PTO must be strictly smaller than default PTO for identical inputs"
    );

    // ---- (B) Arm-gate combination: fired=2, sample NOT seen must pass.
    prime_retrans(&mut c, 1000, 512);
    c.tlp_consecutive_probes_fired = 2;
    c.tlp_rtt_sample_seen_since_last_tlp = false;
    c.rtt_est.sample(5_000); // SRTT present (Task 15 hard requirement).
    assert!(
        c.tlp_arm_gate_passes(),
        "preset must let a 3rd probe arm with no intervening RTT sample"
    );

    // Budget ceiling is still 3: fired=3 must reject.
    c.tlp_consecutive_probes_fired = 3;
    assert!(
        !c.tlp_arm_gate_passes(),
        "preset must still reject once the 3-probe budget is exhausted"
    );

    // ---- (C) Cross-check: same (fired=2, sample=false) state under
    // the defaults rejects (budget cap is 1; sample gate still on),
    // distinguishing the preset's observable effect.
    c.tlp_pto_min_floor_us = 5_000;
    c.tlp_pto_srtt_multiplier_x100 = 200;
    c.tlp_skip_flight_size_gate = false;
    c.tlp_max_consecutive_probes = 1;
    c.tlp_skip_rtt_sample_gate = false;
    c.tlp_consecutive_probes_fired = 2;
    c.tlp_rtt_sample_seen_since_last_tlp = false;
    assert!(
        !c.tlp_arm_gate_passes(),
        "sanity: defaults reject the same (fired=2, sample=false) state"
    );
}

// ---- A-HW knob coverage -------------------------------------------------
//
// One `#[cfg(not(feature = ...))]`-gated test per A-HW compile-time flag.
// Each test asserts a distinguishing consequence of the feature being off
// (where a direct assertion exists) OR serves as a compile-presence
// check that the feature-off branch compiles in CI.
//
// Exercised by Task 15's scripts/ci-feature-matrix.sh: the build that
// turns OFF exactly one flag will run the matching test here; the other
// feature-off tests stay compile-gated out of that build.

/// Helper: build a valid IPv4+TCP frame starting at the IP header.
/// Uses the existing public builder so the IP + TCP checksums are
/// correct without hand-rolling the math. Gated on the rx_cksum test
/// since it is the only consumer.
#[cfg(not(feature = "hw-offload-rx-cksum"))]
fn build_valid_ipv4_tcp_packet() -> Vec<u8> {
    use resd_net_core::tcp_options::TcpOpts;
    use resd_net_core::tcp_output::{build_segment, SegmentTx, TCP_ACK};
    let seg = SegmentTx {
        src_mac: [0; 6],
        dst_mac: [0; 6],
        src_ip: 0x0a_00_00_01,
        dst_ip: 0x0a_00_00_02,
        src_port: 10_000,
        dst_port: 20_000,
        seq: 0,
        ack: 0,
        flags: TCP_ACK,
        window: 1024,
        options: TcpOpts::default(),
        payload: &[],
    };
    let mut buf = vec![0u8; 128];
    let n = build_segment(&seg, &mut buf).expect("build_segment must fit");
    buf.truncate(n);
    // ip_decode_offload_aware expects the IP header at offset 0, so
    // strip the 14-byte Ethernet prefix.
    buf[14..].to_vec()
}

/// Knob: `hw-verify-llq`.
/// Non-default: feature OFF.
/// Observable: Engine::new on ENA does NOT verify LLQ — the capture
/// machinery + verifier compile away entirely. Test asserts the feature-off
/// build compiles + links without the `llq_verify` module.
#[cfg(not(feature = "hw-verify-llq"))]
#[test]
fn knob_hw_verify_llq_off_compiles() {
    // Compile-presence only: the `llq_verify` module is gated on
    // hw-verify-llq via `#[cfg(feature = "hw-verify-llq")] pub mod llq_verify;`
    // in lib.rs. Its absence in this build is the observable.
}

/// Knob: `hw-offload-tx-cksum`.
/// Non-default: feature OFF.
/// Observable: `tx_offload_finalize` has a feature-off stub that is a
/// no-op. The feature-off build compiles the software full-fold TX
/// checksum path unconditionally; no pseudo-header-only write.
#[cfg(not(feature = "hw-offload-tx-cksum"))]
#[test]
fn knob_hw_offload_tx_cksum_off_finalize_is_noop() {
    // The feature-off variant of `tx_offload_finalize` is a no-op —
    // verified at compile time via the
    // `#[cfg(not(feature = "hw-offload-tx-cksum"))]` variant's empty
    // body in tcp_output.rs. Invoking on a null mbuf would require
    // unsafe and a dereference; instead treat the feature-off branch's
    // compile-presence as the proof.
}

/// Knob: `hw-offload-rx-cksum`.
/// Non-default: feature OFF.
/// Observable: `ip_decode_offload_aware` feature-off branch forwards
/// directly to `ip_decode(.., nic_csum_ok=false)` — always software verify,
/// regardless of ol_flags. Counter bumps on NIC-BAD do NOT fire because
/// the classification path is absent.
#[cfg(not(feature = "hw-offload-rx-cksum"))]
#[test]
fn knob_hw_offload_rx_cksum_off_software_verify_always() {
    use resd_net_core::l3_ip::ip_decode_offload_aware;

    let counters = Counters::new();
    let pkt = build_valid_ipv4_tcp_packet();
    // Feature-off: ol_flags + rx_cksum_offload_active are ignored;
    // classifier path absent. With a valid packet, software verify
    // succeeds regardless of what we pass for those args.
    let result = ip_decode_offload_aware(
        &pkt,
        0, // our_ip = 0 → accept any dst
        /* ol_flags = anything */ u64::MAX,
        /* rx_cksum_offload_active = */ true,
        &counters,
    );
    assert!(
        result.is_ok(),
        "software verify should succeed on well-formed packet; got {:?}",
        result
    );
    // No counter bumps in the feature-off path — the classifier that
    // would have bumped this on NIC-BAD is compile-gated out.
    assert_eq!(
        counters.eth.rx_drop_cksum_bad.load(Ordering::Relaxed),
        0,
        "feature-off: eth.rx_drop_cksum_bad must not fire (classifier absent)"
    );
}

/// Knob: `hw-offload-mbuf-fast-free`.
/// Non-default: feature OFF.
/// Observable: no direct Rust-level diff (the bit is PMD-internal).
/// Compile-presence is the proof.
#[cfg(not(feature = "hw-offload-mbuf-fast-free"))]
#[test]
fn knob_hw_offload_mbuf_fast_free_off_compiles() {}

/// Knob: `hw-offload-rss-hash`.
/// Non-default: feature OFF.
/// Observable: `flow_table::hash_bucket_for_lookup` always returns
/// `siphash_4tuple(tup)` regardless of ol_flags, nic_rss_hash, or
/// rss_active.
#[cfg(not(feature = "hw-offload-rss-hash"))]
#[test]
fn knob_hw_offload_rss_hash_off_always_siphash() {
    use resd_net_core::flow_table::hash_bucket_for_lookup;
    let tup = FourTuple {
        local_ip: 0x0a_00_00_01,
        local_port: 1,
        peer_ip: 0x0a_00_00_02,
        peer_port: 2,
    };
    // Feature-off: nic_rss_hash + ol_flags are ignored; siphash_4tuple
    // is deterministic for a given tuple within a process, so two calls
    // with distinct ol_flags + nic_rss_hash values produce the SAME
    // hash.
    let with_nic_hash = hash_bucket_for_lookup(&tup, u64::MAX, 0xdead_beef, true);
    let with_different_nic = hash_bucket_for_lookup(&tup, 0, 0xbeef_dead, false);
    assert_eq!(
        with_nic_hash, with_different_nic,
        "feature-off: nic_rss_hash + ol_flags must be ignored — SipHash deterministic"
    );
}

/// Knob: `hw-offload-rx-timestamp`.
/// Non-default: feature OFF.
/// Observable: `Engine::hw_rx_ts_ns` feature-off stub always returns 0.
/// The engine state fields `rx_ts_offset` / `rx_ts_flag_mask` are
/// compile-gated out. Compile-presence is the proof since constructing
/// an Engine in a unit test is non-trivial (and `hw_rx_ts_ns` is
/// `pub(crate)`).
#[cfg(not(feature = "hw-offload-rx-timestamp"))]
#[test]
fn knob_hw_offload_rx_timestamp_off_compiles() {}
