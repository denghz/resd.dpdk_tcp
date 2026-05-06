//! Pressure Suite — `pressure-reassembly-saturation-smoke`.
//! A11.3 Lane A.
//!
//! Workload: open one connection via the test-server bypass, inject one
//! in-order payload segment (delivered immediately via deliver_readable),
//! then drive a series of out-of-order (OOO) frames that fill the reorder
//! queue.  Once `emit_ack` shrinks `rcv_wnd` in response to a growing
//! reorder queue, additional OOO frames fall out-of-window and are
//! silently discarded — this exercises the window-contraction / OOO-reject
//! path without a RST.
//!
//! Note on recv_buf_drops: in the test-server bypass path,
//! deliver_readable drains recv.bytes synchronously after each
//! inject_rx_frame call, so recv.bytes is always empty between frames.
//! recv_buf_drops can only fire if a single in-order frame's payload
//! exceeds the cap, which does not apply here.  OOO frames that arrive
//! after rcv_wnd has shrunk are rejected by the window check before
//! reaching the cap-full path.
//!
//! Engine config:
//!   * `recv_buffer_bytes = 4096` — small cap so the reorder queue
//!     exhausts the advertised window after a few OOO frames.
//!   * `max_connections = 4` — only one conn needed.
//!   * `tcp_msl_ms = 100` — fast TIME_WAIT for teardown.
//!
//! Counters asserted (deltas across the workload):
//!   * `tcp.recv_buf_delivered` > 0  — the in-order segment was delivered.
//!   * `tcp.rx_reassembly_queued` > 0  — OOO bytes were enqueued.
//!   * `tcp.rx_reassembly_hole_filled` == 0  — no gap was ever closed
//!       (no in-order arrival after the OOO storm).
//!   * `tcp.mbuf_refcnt_drop_unexpected` == 0  — refcount accounting clean.
//!   * `obs.events_dropped` == 0  — event-queue cap not breached.
//!   * `tcp.tx_rst` == 0  — OOO discard and window contraction do not RST.
//!   * `tcp.rx_mempool_avail`, `tcp.tx_data_mempool_avail` both ±32.
//!
//! Gated behind `all(feature = "pressure-test", feature = "test-server")`.
//! Uses the test-server bypass (port_id = u16::MAX); no real NIC or TAP.

#![cfg(all(feature = "pressure-test", feature = "test-server"))]

mod common;

use common::pressure::{assert_delta, CounterSnapshot, PressureBucket, Relation};
use common::{build_tcp_frame, OUR_IP, PEER_IP};
use dpdk_net_core::engine::EngineConfig;
use dpdk_net_core::tcp_options::TcpOpts;
use dpdk_net_core::tcp_output::{TCP_ACK, TCP_PSH};

/// Recv-buffer byte cap.  Must be > 3 × SEG_BYTES so the first 3 OOO
/// frames fit, leaving the 4th frame to trigger cap-drop.
const RECV_BUF: u32 = 4_096;
/// Payload bytes per injected segment.  Must be < RECV_BUF / 3.
const SEG_BYTES: usize = 1_024;
/// Mempool drift tolerance.
const POOL_DRIFT: i64 = 32;

#[test]
fn pressure_reassembly_saturation_smoke() {
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
        recv_buffer_bytes: RECV_BUF,
        ..Default::default()
    });

    let bucket = PressureBucket::open(
        "pressure-reassembly-saturation",
        "smoke",
        h.eng.counters(),
    );

    // ── Phase 1: passive open ──────────────────────────────────────────
    //
    // do_passive_open: listen→SYN→SYN-ACK→final-ACK.
    // After: rcv_nxt = peer_iss + 1 = 0x10000001, peer_seq = 0x10000001,
    //        recv.bytes empty, free_space_total = RECV_BUF = 4096.
    let conn = h.do_passive_open();
    let _ = drain_tx_frames();

    // ── Phase 2: in-order fill (one SEG_BYTES segment) ─────────────────
    //
    // inject_peer_data injects at seq = peer_seq.  In the test-server
    // bypass, deliver_readable drains recv.bytes synchronously after each
    // inject_rx_frame call.
    //
    // After: recv.bytes = 0 (drained), rcv_nxt = 0x10000401,
    //        free_space_total = RECV_BUF = 4096.
    h.inject_peer_data(&vec![0x55u8; SEG_BYTES]);
    let _ = drain_tx_frames();
    h.eng.drain_events(64, |_, _| {});

    // peer_seq now equals rcv_nxt (= 0x10000401).
    let rcv_nxt = h.peer_seq.get();

    // ── Phase 3: OOO frames — fill reassembly queue ────────────────────
    //
    // Inject 3 OOO frames, each SEG_BYTES (1024) bytes, starting at
    // rcv_nxt+1 (one byte past the in-order delivery point).  This
    // creates a 1-byte gap, preventing the gap-fill path from triggering.
    //
    // recv.bytes = 0 throughout (drained synchronously by deliver_readable).
    // Reorder-queue accounting (cap = RECV_BUF = 4096):
    //   OOO-0: reorder = 1024, free_total = 4096 − 1024 = 3072 → rcv_wnd = 3072
    //   OOO-1: reorder = 2048, free_total = 4096 − 2048 = 2048 → rcv_wnd = 2048
    //   OOO-2: seq offset = 2049 ≥ rcv_wnd=2048 → out-of-window, REJECTED
    //
    // Only OOO-0 and OOO-1 are accepted; rx_reassembly_queued += 2048.
    set_virt_ns(4_000_000);
    for i in 0u32..3 {
        let ooo_seq = rcv_nxt.wrapping_add(1).wrapping_add(i * SEG_BYTES as u32);
        let frame = build_tcp_frame(
            PEER_IP,
            40_000,
            OUR_IP,
            5555,
            ooo_seq,
            h.our_iss.get().wrapping_add(1),
            TCP_ACK | TCP_PSH,
            u16::MAX,
            TcpOpts::default(),
            &vec![0xabu8; SEG_BYTES],
        );
        h.eng.inject_rx_frame(&frame).expect("inject OOO");
        let _ = drain_tx_frames();
        h.eng.drain_events(64, |_, _| {});
    }
    // After loop: reorder = 2048, free_space_total = 2048 (OOO-2 rejected).

    // ── Phase 4: additional OOO frame (out-of-window after rcv_wnd shrink) ──
    //
    // After OOO-0 and OOO-1 filled the reorder queue to 2×SEG_BYTES,
    // emit_ack shrinks rcv_wnd to free_space_total = RECV_BUF - 2×SEG_BYTES.
    // This frame's seq offset (3×SEG_BYTES + 1) exceeds the shrunken window
    // and is silently dropped by the window check — no RST, no mbuf leak.
    set_virt_ns(5_000_000);
    {
        let overflow_seq = rcv_nxt.wrapping_add(1).wrapping_add(3 * SEG_BYTES as u32);
        let frame = build_tcp_frame(
            PEER_IP,
            40_000,
            OUR_IP,
            5555,
            overflow_seq,
            h.our_iss.get().wrapping_add(1),
            TCP_ACK | TCP_PSH,
            u16::MAX,
            TcpOpts::default(),
            &[0xddu8; 64],
        );
        h.eng.inject_rx_frame(&frame).expect("inject overflow OOO");
        let _ = drain_tx_frames();
        h.eng.drain_events(64, |_, _| {});
    }

    // ── Settle ─────────────────────────────────────────────────────────
    let _ = drain_tx_frames();

    // ── Snapshot + assertions ──────────────────────────────────────────
    let after = CounterSnapshot::capture(h.eng.counters());
    let delta = after.delta_since(&bucket.before);

    // In-order segment was delivered to the app layer.
    assert_delta(&delta, "tcp.recv_buf_delivered", Relation::Gt(0));

    // OOO bytes were enqueued in the reassembly queue across the storm.
    assert_delta(&delta, "tcp.rx_reassembly_queued", Relation::Gt(0));

    // No gap was ever closed: no in-order segment arrived after the OOO
    // storm to fill the 1-byte gap at rcv_nxt+1.
    assert_delta(&delta, "tcp.rx_reassembly_hole_filled", Relation::Eq(0));

    // Mbuf refcount accounting remained clean: cap-drop rollback did not
    // leave an unexpected extra decrement, and no other path did either.
    assert_delta(&delta, "tcp.mbuf_refcnt_drop_unexpected", Relation::Eq(0));

    // Event-queue cap was not breached.
    assert_delta(&delta, "obs.events_dropped", Relation::Eq(0));

    // OOO discard and window contraction must not produce RSTs.
    assert_delta(&delta, "tcp.tx_rst", Relation::Eq(0));

    // Mempool drift: both pools must round-trip to ±POOL_DRIFT of baseline
    // once the storm settles — no sustained leak in either direction.
    assert_delta(
        &delta,
        "tcp.rx_mempool_avail",
        Relation::Range(-POOL_DRIFT, POOL_DRIFT),
    );
    assert_delta(
        &delta,
        "tcp.tx_data_mempool_avail",
        Relation::Range(-POOL_DRIFT, POOL_DRIFT),
    );

    // Close the connection to allow the engine to reap its TcpConn slot
    // and release the reorder-queue mbufs before harness teardown.
    let _ = h.eng.close_conn(conn);
    let _ = drain_tx_frames();
    h.eng.drain_events(64, |_, _| {});

    bucket.finish_ok();
}
