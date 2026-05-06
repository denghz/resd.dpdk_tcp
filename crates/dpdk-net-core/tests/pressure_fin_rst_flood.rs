//! Pressure Suite — `pressure-fin-rst-flood`.
//! A11.4 Lane A.  Two sub-suites in one file.
//!
//! **Sub-suite 10a — `unmatched_rst_flood`:**
//! Open N_LEGIT = 10 passive connections, then inject N_FLOOD = 100 000
//! RST frames from FLOOD_PEER_IP (10.99.2.3 — a source IP that has no
//! open connection).  The engine must route every RST to the unmatched
//! path (`tcp.rx_unmatched++`) without touching any of the 10 live
//! connections.  The engine does not reply to incoming RSTs
//! (`send_rst_unmatched` short-circuits on TCP_RST), so the TX intercept
//! queue stays empty throughout the flood.
//!
//! Counters asserted (deltas across the flood):
//!   * `tcp.rx_unmatched` >= N_FLOOD — every injected RST was unmatched.
//!   * `tcp.conn_close` == 0 — no live connection was terminated.
//!   * `obs.events_dropped` == 0 — event-queue cap not breached.
//!   * `tcp.mbuf_refcnt_drop_unexpected` == 0.
//!
//! **Sub-suite 10b — `matched_rst_flood`:**
//! Open N_MATCHED = 64 passive connections.  Inject one RST per
//! connection at the exact 4-tuple (PEER_IP:src_port_i ↔
//! OUR_IP:OUR_PORT_10B) with seq = PEER_ISS_10B + 1 (= conn.rcv_nxt
//! immediately post-3WHS).  Each RST terminates exactly one connection.
//!
//! Counters asserted (deltas):
//!   * `tcp.rx_rst` >= N_MATCHED — every injected RST matched an open conn.
//!   * `tcp.conn_rst` >= N_MATCHED — every match closed the conn via RST.
//!   * `tcp.conn_close` >= N_MATCHED — every closed conn bumped conn_close.
//!   * `obs.events_dropped` == 0.
//!   * `tcp.mbuf_refcnt_drop_unexpected` == 0.
//!   * Flow table drained: `engine.flow_table().active_conns() == 0`.
//!
//! Both sub-suites use the test-server bypass (port_id = u16::MAX); no
//! real NIC or TAP required.
//!
//! Gated behind `all(feature = "pressure-test", feature = "test-server")`.

#![cfg(all(feature = "pressure-test", feature = "test-server"))]

mod common;

use common::pressure::{assert_delta, CounterSnapshot, PressureBucket, Relation};
use common::{build_tcp_ack, build_tcp_frame, parse_syn_ack, OUR_IP, PEER_IP};
use dpdk_net_core::engine::EngineConfig;
use dpdk_net_core::tcp_options::TcpOpts;
use dpdk_net_core::tcp_output::{TCP_RST, TCP_SYN};

/// Source IP for unmatched flood RSTs — not used by any legit connection.
/// (10.99.2.3: same /24 as PEER_IP=10.99.2.1 / OUR_IP=10.99.2.2.)
const FLOOD_PEER_IP: u32 = 0x0a_63_02_03; // 10.99.2.3

// ─────────────────────────────────────────────────────────────────────────────
// Sub-suite 10a — unmatched-rst-flood constants
// ─────────────────────────────────────────────────────────────────────────────

const N_FLOOD: u32 = 100_000;
const N_LEGIT: u32 = 10;
const OUR_PORT_10A: u16 = 7_001;
const LEGIT_BASE_10A: u16 = 50_000;
const PEER_ISS_10A: u32 = 0x1000_0000;

// ─────────────────────────────────────────────────────────────────────────────
// Sub-suite 10b — matched-rst-flood constants
// ─────────────────────────────────────────────────────────────────────────────

const N_MATCHED: u32 = 64;
const OUR_PORT_10B: u16 = 7_002;
const LEGIT_BASE_10B: u16 = 50_000;
const PEER_ISS_10B: u32 = 0x2000_0000;

// ─────────────────────────────────────────────────────────────────────────────
// Helper: perform a full 3-way handshake for one passive connection.
// Returns the ConnHandle.  Does NOT update CovHarness tracking fields.
// ─────────────────────────────────────────────────────────────────────────────
fn open_one_passive(
    eng: &dpdk_net_core::engine::Engine,
    listen_h: dpdk_net_core::test_server::ListenHandle,
    src_port: u16,
    peer_iss: u32,
    our_port: u16,
) -> dpdk_net_core::flow_table::ConnHandle {
    use dpdk_net_core::test_tx_intercept::drain_tx_frames;

    let syn = build_tcp_frame(
        PEER_IP, src_port, OUR_IP, our_port,
        peer_iss, 0, TCP_SYN, u16::MAX,
        { let mut o = TcpOpts::default(); o.mss = Some(1460); o },
        &[],
    );
    eng.inject_rx_frame(&syn).expect("inject SYN");
    let frames = drain_tx_frames();
    assert_eq!(frames.len(), 1, "expected SYN-ACK for src_port={src_port}");
    let (our_iss, _) = parse_syn_ack(&frames[0]).expect("parse SYN-ACK");

    let ack = build_tcp_ack(
        PEER_IP, src_port, OUR_IP, our_port,
        peer_iss.wrapping_add(1),
        our_iss.wrapping_add(1),
    );
    eng.inject_rx_frame(&ack).expect("inject final ACK");
    let _ = drain_tx_frames();

    eng.accept_next(listen_h).expect("accept_next")
}

// ─────────────────────────────────────────────────────────────────────────────
// Sub-suite 10a — unmatched-rst-flood
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn pressure_unmatched_rst_flood() {
    use common::CovHarness;
    use dpdk_net_core::clock::set_virt_ns;
    use dpdk_net_core::test_tx_intercept::drain_tx_frames;

    let h = CovHarness::new_with_config(EngineConfig {
        port_id: u16::MAX,
        local_ip: OUR_IP,
        gateway_ip: PEER_IP,
        gateway_mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x02],
        tcp_mss: 1460,
        max_connections: 32,
        tcp_msl_ms: 100,
        ..Default::default()
    });

    let bucket = PressureBucket::open(
        "pressure-fin-rst-flood",
        "unmatched_rst_flood",
        h.eng.counters(),
    );

    // ── Phase 1: open N_LEGIT passive connections ──────────────────────
    //
    // Each conn uses a distinct source port; all share the same PEER_ISS.
    set_virt_ns(1_000_000);
    let listen_h = h.eng.listen(OUR_IP, OUR_PORT_10A).expect("listen");
    let _ = drain_tx_frames();

    let mut conn_handles = Vec::with_capacity(N_LEGIT as usize);
    for i in 0..N_LEGIT {
        let src_port = LEGIT_BASE_10A.wrapping_add(i as u16);
        let conn = open_one_passive(&h.eng, listen_h, src_port, PEER_ISS_10A, OUR_PORT_10A);
        conn_handles.push(conn);
    }

    assert_eq!(
        h.eng.flow_table().active_conns(),
        N_LEGIT as usize,
        "expected {N_LEGIT} active conns before flood"
    );

    // ── Phase 2: inject N_FLOOD unmatched RSTs ─────────────────────────
    //
    // RSTs from FLOOD_PEER_IP (10.99.2.3) — no connection has this source
    // IP, so every frame goes to rx_unmatched path.  The engine does not
    // reply to incoming RSTs, so the TX intercept queue stays empty;
    // periodic poll_once + drain_tx_frames is a safety measure only.
    set_virt_ns(2_000_000);
    for i in 0..N_FLOOD {
        let rst = build_tcp_frame(
            FLOOD_PEER_IP,
            (i % 65534) as u16 + 1, // src_port: cycles 1..=65534
            OUR_IP,
            OUR_PORT_10A,
            PEER_ISS_10A,
            0,
            TCP_RST,
            0,
            TcpOpts::default(),
            &[],
        );
        h.eng.inject_rx_frame(&rst).expect("inject flood RST");
        if i % 10_000 == 9_999 {
            h.eng.poll_once();
            let _ = drain_tx_frames();
            h.eng.drain_events(64, |_, _| {});
        }
    }
    h.eng.poll_once();
    let _ = drain_tx_frames();
    h.eng.drain_events(64, |_, _| {});

    // ── Snapshot + assertions ──────────────────────────────────────────
    let after = CounterSnapshot::capture(h.eng.counters());
    let delta = after.delta_since(&bucket.before);

    // Every flood RST went to the unmatched path.
    assert_delta(&delta, "tcp.rx_unmatched", Relation::Ge(N_FLOOD as i64));

    // No live connection was closed by the unmatched RSTs.
    assert_delta(&delta, "tcp.conn_close", Relation::Eq(0));

    // Event-queue cap not breached.
    assert_delta(&delta, "obs.events_dropped", Relation::Eq(0));

    // Mbuf refcount accounting clean throughout.
    assert_delta(&delta, "tcp.mbuf_refcnt_drop_unexpected", Relation::Eq(0));

    // All N_LEGIT connections survive the flood.
    let active = h.eng.flow_table().active_conns();
    assert_eq!(
        active,
        N_LEGIT as usize,
        "unmatched_rst_flood: {active} active conns post-flood, expected {N_LEGIT}"
    );

    // Close legit conns before bucket finish so harness teardown is clean.
    for conn in conn_handles {
        let _ = h.eng.close_conn(conn);
    }
    h.eng.poll_once();
    let _ = drain_tx_frames();
    h.eng.drain_events(64, |_, _| {});

    bucket.finish_ok();
}

// ─────────────────────────────────────────────────────────────────────────────
// Sub-suite 10b — matched-rst-flood
// ─────────────────────────────────────────────────────────────────────────────

#[test]
fn pressure_matched_rst_flood() {
    use common::CovHarness;
    use dpdk_net_core::clock::set_virt_ns;
    use dpdk_net_core::test_tx_intercept::drain_tx_frames;

    let h = CovHarness::new_with_config(EngineConfig {
        port_id: u16::MAX,
        local_ip: OUR_IP,
        gateway_ip: PEER_IP,
        gateway_mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x02],
        tcp_mss: 1460,
        max_connections: 128,
        tcp_msl_ms: 100,
        ..Default::default()
    });

    let bucket = PressureBucket::open(
        "pressure-fin-rst-flood",
        "matched_rst_flood",
        h.eng.counters(),
    );

    // ── Phase 1: open N_MATCHED passive connections ────────────────────
    //
    // Each conn at (PEER_IP:src_port_i ↔ OUR_IP:OUR_PORT_10B) with
    // PEER_ISS_10B.  After the 3WHS, conn.rcv_nxt = PEER_ISS_10B + 1.
    set_virt_ns(1_000_000);
    let listen_h = h.eng.listen(OUR_IP, OUR_PORT_10B).expect("listen");
    let _ = drain_tx_frames();

    let mut src_ports: Vec<u16> = Vec::with_capacity(N_MATCHED as usize);
    for i in 0..N_MATCHED {
        let src_port = LEGIT_BASE_10B.wrapping_add(i as u16);
        open_one_passive(&h.eng, listen_h, src_port, PEER_ISS_10B, OUR_PORT_10B);
        src_ports.push(src_port);
    }

    assert_eq!(
        h.eng.flow_table().active_conns(),
        N_MATCHED as usize,
        "expected {N_MATCHED} active conns before RST storm"
    );

    // ── Phase 2: inject one matched RST per connection ─────────────────
    //
    // For each conn, inject RST with seq = PEER_ISS_10B + 1 (= conn.rcv_nxt).
    // RFC 9293 §3.9: seq is accepted when seq ∈ [rcv_nxt, rcv_nxt+rcv_wnd).
    // seq == rcv_nxt satisfies this check (0 < rcv_wnd always).
    set_virt_ns(2_000_000);
    for &src_port in &src_ports {
        let rst = build_tcp_frame(
            PEER_IP,
            src_port,
            OUR_IP,
            OUR_PORT_10B,
            PEER_ISS_10B.wrapping_add(1),
            0,
            TCP_RST,
            0,
            TcpOpts::default(),
            &[],
        );
        h.eng.inject_rx_frame(&rst).expect("inject matched RST");
    }
    h.eng.poll_once();
    let _ = drain_tx_frames();
    h.eng.drain_events(256, |_, _| {});

    // ── Snapshot + assertions ──────────────────────────────────────────
    let after = CounterSnapshot::capture(h.eng.counters());
    let delta = after.delta_since(&bucket.before);

    // Every injected RST matched an open connection.
    assert_delta(&delta, "tcp.rx_rst", Relation::Ge(N_MATCHED as i64));

    // Every matched RST terminated its connection via RST.
    assert_delta(&delta, "tcp.conn_rst", Relation::Ge(N_MATCHED as i64));

    // Every RST-closed conn also bumped the general close counter.
    assert_delta(&delta, "tcp.conn_close", Relation::Ge(N_MATCHED as i64));

    // Event-queue cap not breached.
    assert_delta(&delta, "obs.events_dropped", Relation::Eq(0));

    // Mbuf refcount accounting clean.
    assert_delta(&delta, "tcp.mbuf_refcnt_drop_unexpected", Relation::Eq(0));

    // All N_MATCHED connections removed from the flow table.
    let active = h.eng.flow_table().active_conns();
    assert_eq!(
        active,
        0,
        "matched_rst_flood: {active} active conns remain, expected 0"
    );

    bucket.finish_ok();
}
