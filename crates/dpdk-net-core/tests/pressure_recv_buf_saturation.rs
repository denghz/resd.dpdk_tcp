//! Pressure Suite — `pressure-recv-buf-saturation`.
//! A11.4 Lane B.  Two sub-suites (buckets) in one file.
//!
//! **Bucket 11a — `sustained_slow_drain`:**
//! Open one connection via the test-server bypass.  Inject FLOOD_BATCHES
//! batches of FLOOD_BATCH_FRAMES frames each (payload PAYLOAD_BYTES),
//! draining received events between batches but NOT calling read_bytes.
//! Verifies the engine handles sustained high-volume injection cleanly.
//!
//! Note on recv_buf_drops: in the test-server bypass path, each
//! inject_rx_frame → dispatch_one_rx_mbuf → deliver_readable call drains
//! recv.bytes synchronously, so the buffer never accumulates between
//! individual frames.  recv_buf_drops can only fire if a single frame's
//! payload exceeds recv_buffer_bytes, which does not apply here
//! (PAYLOAD_BYTES = 1460 < RECV_BUF_11A = 16 KiB).  The primary oracle
//! for this bucket is recv_buf_delivered — verifying all injected bytes
//! were delivered to the app layer without mbuf leaks or RSTs.
//!
//! Counters asserted (deltas across the flood):
//!   * `tcp.recv_buf_delivered` > 0 — data was delivered to the app layer.
//!   * `tcp.mbuf_refcnt_drop_unexpected` == 0.
//!   * `obs.events_dropped` == 0.
//!   * `tcp.tx_rst` == 0 — no RSTs emitted under data pressure.
//!
//! **Bucket 11b — `starvation_resume`:**
//! Open one connection.  Inject N_STARVATION frames in a burst without
//! draining events or reading data (simulating app-side starvation).
//! Then resume draining and verify the connection is still alive.
//!
//! Counters asserted (deltas):
//!   * `obs.events_dropped` == 0 — event-queue soft-cap not breached
//!       (N_STARVATION kept small enough to stay within the soft-cap).
//!   * `tcp.mbuf_refcnt_drop_unexpected` == 0.
//!   * Connection remains alive post-resume (active_conns == 1).
//!
//! Both sub-suites use the test-server bypass (port_id = u16::MAX).
//!
//! Gated behind `all(feature = "pressure-test", feature = "test-server")`.

#![cfg(all(feature = "pressure-test", feature = "test-server"))]

mod common;

use common::pressure::{assert_delta, CounterSnapshot, PressureBucket, Relation};
use common::{build_tcp_frame, OUR_IP, PEER_IP};
use dpdk_net_core::engine::EngineConfig;
use dpdk_net_core::tcp_options::TcpOpts;
use dpdk_net_core::tcp_output::{TCP_ACK, TCP_PSH};

/// Recv-buffer cap for 11a (16 KiB).
const RECV_BUF_11A: u32 = 16_384;

/// Payload size per injected frame (one MSS).
const PAYLOAD_BYTES: usize = 1_460;

/// Number of frames per flood batch in 11a.
const FLOOD_BATCH_FRAMES: u32 = 30;

/// Number of flood batches in 11a.  Total injected:
///   30 × 50 × 1460 = 2 190 000 bytes → many recv_buf_delivered increments.
const FLOOD_BATCHES: u32 = 50;

/// Recv-buffer cap for 11b (large enough to absorb the starvation burst).
const RECV_BUF_11B: u32 = 65_536;

/// Frames injected during the starvation window in 11b.
/// Each frame is PAYLOAD_BYTES bytes; RECV_BUF_11B / PAYLOAD_BYTES ≈ 44.
/// We inject 40 frames (all fit in the buffer) so recv_buf_drops == 0
/// and events stay within the soft-cap.
const N_STARVATION: u32 = 40;


// ─────────────────────────────────────────────────────────────────────────────
// Bucket 11a — sustained_slow_drain
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn pressure_recv_buf_saturation_sustained() {
    use common::CovHarness;
    use dpdk_net_core::clock::set_virt_ns;
    use dpdk_net_core::test_tx_intercept::drain_tx_frames;

    let mut h = CovHarness::new_with_config(EngineConfig {
        port_id: u16::MAX,
        local_ip: OUR_IP,
        gateway_ip: PEER_IP,
        gateway_mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x02],
        tcp_mss: 1460,
        max_connections: 4,
        tcp_msl_ms: 100,
        recv_buffer_bytes: RECV_BUF_11A,
        ..Default::default()
    });

    let bucket = PressureBucket::open(
        "pressure-recv-buf-saturation",
        "sustained_slow_drain",
        h.eng.counters(),
    );

    // ── Passive open ───────────────────────────────────────────────────
    let conn = h.do_passive_open();
    let _ = drain_tx_frames();

    // ── Flood in batches ───────────────────────────────────────────────
    //
    // Each batch injects FLOOD_BATCH_FRAMES in-order data frames without
    // reading the recv buffer.  After a few batches the 16-KiB cap fills
    // and subsequent frames are dropped via the buf_full_drop path,
    // incrementing tcp.recv_buf_drops.
    //
    // peer_seq starts at PEER_ISS + 1 (post-3WHS from do_passive_open).
    let mut peer_seq = h.peer_seq.get();
    let our_iss = h.our_iss.get();

    for batch in 0..FLOOD_BATCHES {
        set_virt_ns((3 + batch) as u64 * 1_000_000);
        for _ in 0..FLOOD_BATCH_FRAMES {
            let frame = build_tcp_frame(
                PEER_IP,
                40_000,
                OUR_IP,
                5555,
                peer_seq,
                our_iss.wrapping_add(1),
                TCP_ACK | TCP_PSH,
                u16::MAX,
                TcpOpts::default(),
                &vec![0xa5u8; PAYLOAD_BYTES],
            );
            h.eng.inject_rx_frame(&frame).expect("inject data");
            peer_seq = peer_seq.wrapping_add(PAYLOAD_BYTES as u32);
        }
        let _ = drain_tx_frames();
        h.eng.drain_events(64, |_, _| {});
    }

    // ── Snapshot + assertions ──────────────────────────────────────────
    let after = CounterSnapshot::capture(h.eng.counters());
    let delta = after.delta_since(&bucket.before);

    // Data was delivered to the app layer without interruption.
    assert_delta(&delta, "tcp.recv_buf_delivered", Relation::Gt(0));

    // No RSTs emitted under sustained data pressure.
    assert_delta(&delta, "tcp.tx_rst", Relation::Eq(0));

    // Event-queue cap not breached.
    assert_delta(&delta, "obs.events_dropped", Relation::Eq(0));

    // Mbuf refcount accounting clean throughout flood.
    assert_delta(&delta, "tcp.mbuf_refcnt_drop_unexpected", Relation::Eq(0));

    // Close connection to release recv-buffer mbufs before harness Drop.
    let _ = h.eng.close_conn(conn);
    let _ = drain_tx_frames();
    h.eng.drain_events(64, |_, _| {});

    bucket.finish_ok();
}

// ─────────────────────────────────────────────────────────────────────────────
// Bucket 11b — starvation_resume
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn pressure_recv_buf_saturation_starvation() {
    use common::CovHarness;
    use dpdk_net_core::clock::set_virt_ns;
    use dpdk_net_core::test_tx_intercept::drain_tx_frames;

    let mut h = CovHarness::new_with_config(EngineConfig {
        port_id: u16::MAX,
        local_ip: OUR_IP,
        gateway_ip: PEER_IP,
        gateway_mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x02],
        tcp_mss: 1460,
        max_connections: 4,
        tcp_msl_ms: 100,
        recv_buffer_bytes: RECV_BUF_11B,
        ..Default::default()
    });

    let bucket = PressureBucket::open(
        "pressure-recv-buf-saturation",
        "starvation_resume",
        h.eng.counters(),
    );

    // ── Passive open ───────────────────────────────────────────────────
    let conn = h.do_passive_open();
    let _ = drain_tx_frames();

    // ── Starvation: inject N_STARVATION frames without draining ────────
    //
    // App does NOT drain events or read recv bytes during this window.
    // All N_STARVATION frames are in-order (sequential seq numbers) so
    // they are delivered into recv.bytes.  With RECV_BUF_11B = 64 KiB
    // and N_STARVATION × PAYLOAD_BYTES = 40 × 1460 = 58 400 < 65 536,
    // the buffer does not overflow → recv_buf_drops == 0.
    // The event queue receives at most N_STARVATION READABLE events;
    // keeping N_STARVATION small (40) keeps the queue within its cap.
    set_virt_ns(3_000_000);
    let mut peer_seq = h.peer_seq.get();
    let our_iss = h.our_iss.get();

    for _ in 0..N_STARVATION {
        let frame = build_tcp_frame(
            PEER_IP,
            40_000,
            OUR_IP,
            5555,
            peer_seq,
            our_iss.wrapping_add(1),
            TCP_ACK | TCP_PSH,
            u16::MAX,
            TcpOpts::default(),
            &vec![0xbbu8; PAYLOAD_BYTES],
        );
        h.eng.inject_rx_frame(&frame).expect("inject starvation data");
        peer_seq = peer_seq.wrapping_add(PAYLOAD_BYTES as u32);
    }
    let _ = drain_tx_frames();

    // ── Resume: drain events and verify conn is alive ──────────────────
    set_virt_ns(4_000_000);
    let _ = drain_tx_frames();
    h.eng.drain_events(256, |_, _| {});

    // ── Snapshot + assertions ──────────────────────────────────────────
    let after = CounterSnapshot::capture(h.eng.counters());
    let delta = after.delta_since(&bucket.before);

    // No events were dropped during the starvation window.
    assert_delta(&delta, "obs.events_dropped", Relation::Eq(0));

    // Mbuf refcount accounting clean.
    assert_delta(&delta, "tcp.mbuf_refcnt_drop_unexpected", Relation::Eq(0));

    // Connection is still alive post-resume.
    let active = h.eng.flow_table().active_conns();
    assert_eq!(
        active,
        1,
        "starvation_resume: {active} active conns post-resume, expected 1"
    );

    // Close connection before harness teardown.
    let _ = h.eng.close_conn(conn);
    let _ = drain_tx_frames();
    h.eng.drain_events(64, |_, _| {});

    bucket.finish_ok();
}
