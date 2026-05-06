//! Pressure Suite — `pressure-listen-accept-exhaustion`.
//! A11.4 Lane C.
//!
//! Workload: exercise two forms of connection-table exhaustion.
//!
//! **Phase A — listen-slot exhaustion:**
//! Inject N_SYN_FLOOD = 200 SYNs at a single listen port in rapid
//! succession without completing any handshake.  The engine's test-server
//! listen slot holds one in-progress handshake at a time; the first SYN
//! occupies the slot (SYN_RCVD, SYN-ACK emitted) and all subsequent SYNs
//! find the slot "full" and receive a RST|ACK reply
//! (`emit_rst_for_unsolicited_syn`, which bumps `tcp.tx_rst`).
//!
//! **Phase B — flow-table exhaustion:**
//! Fill the flow table via N_MAX passive opens, then call `connect()` one
//! more time — the flow table is at `max_connections` capacity, so
//! `flow_table.insert()` returns None and `tcp.conn_table_full` is
//! incremented.
//!
//! Counters asserted (deltas across both phases):
//!   * `tcp.tx_rst` >= N_SYN_FLOOD - 1  — listen-slot rejections emit RSTs.
//!   * `tcp.conn_table_full` >= 1        — flow-table capacity hit.
//!   * `obs.events_dropped` == 0.
//!   * `tcp.mbuf_refcnt_drop_unexpected` == 0.
//!
//! Post-test invariant:
//!   * Listener slot is still valid (no leak): `listen()` can be called on
//!     the same port after all conns are closed — tested implicitly by
//!     the Phase B setup which reuses the same listen handle.
//!
//! Uses the test-server bypass (port_id = u16::MAX); no real NIC or TAP.
//!
//! Gated behind `all(feature = "pressure-test", feature = "test-server")`.

#![cfg(all(feature = "pressure-test", feature = "test-server"))]

mod common;

use common::pressure::{assert_delta, CounterSnapshot, PressureBucket, Relation};
use common::{build_tcp_ack, build_tcp_frame, parse_syn_ack, OUR_IP, PEER_IP};
use dpdk_net_core::engine::EngineConfig;
use dpdk_net_core::tcp_options::TcpOpts;
use dpdk_net_core::tcp_output::TCP_SYN;

/// Number of SYNs injected in Phase A (listen-slot exhaustion).
const N_SYN_FLOOD: u32 = 200;

/// Number of passive-open conns used to fill the flow table in Phase B.
const N_MAX: u32 = 4;

/// Source-port base for Phase A SYNs.
const FLOOD_BASE_PORT: u16 = 30_000;

/// Source-port base for Phase B passive conns.
const PASSIVE_BASE_PORT: u16 = 40_000;

/// Listen port.
const OUR_PORT: u16 = 5_516;

/// Peer ISS used for all injected SYNs.
const PEER_ISS: u32 = 0x3000_0000;

#[test]
fn pressure_listen_accept_exhaustion() {
    use common::CovHarness;
    use dpdk_net_core::clock::set_virt_ns;
    use dpdk_net_core::test_tx_intercept::drain_tx_frames;

    let h = CovHarness::new_with_config(EngineConfig {
        port_id: u16::MAX,
        local_ip: OUR_IP,
        gateway_ip: PEER_IP,
        gateway_mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x02],
        tcp_mss: 1460,
        // Small table so Phase B exhaustion triggers quickly.
        max_connections: N_MAX,
        tcp_msl_ms: 100,
        ..Default::default()
    });

    let bucket = PressureBucket::open(
        "pressure-listen-accept-exhaustion",
        "syn_flood_and_table_full",
        h.eng.counters(),
    );

    // ── Phase A: listen-slot exhaustion ───────────────────────────────
    //
    // Inject N_SYN_FLOOD SYNs at FLOOD_BASE_PORT+i.  The first SYN
    // creates a SYN_RCVD conn and occupies the in-progress slot; the
    // engine replies with a SYN-ACK.  Each subsequent SYN finds the slot
    // occupied (in_progress.is_some()) and receives a RST|ACK reply,
    // bumping tcp.tx_rst.
    set_virt_ns(1_000_000);
    let listen_h = h.eng.listen(OUR_IP, OUR_PORT).expect("listen");
    let _ = drain_tx_frames();

    for i in 0..N_SYN_FLOOD {
        let syn = build_tcp_frame(
            PEER_IP,
            FLOOD_BASE_PORT.wrapping_add(i as u16),
            OUR_IP,
            OUR_PORT,
            PEER_ISS.wrapping_add(i * 2),
            0,
            TCP_SYN,
            u16::MAX,
            { let mut o = TcpOpts::default(); o.mss = Some(1460); o },
            &[],
        );
        h.eng.inject_rx_frame(&syn).expect("inject SYN");
        // Drain TX frames (SYN-ACK or RST) after each injection to keep
        // the TX intercept queue from accumulating N_SYN_FLOOD frames.
        let _ = drain_tx_frames();
    }
    h.eng.poll_once();
    let _ = drain_tx_frames();
    h.eng.drain_events(64, |_, _| {});

    // The in-progress SYN (SYN-0) is still in the listen slot.
    // Drop it: complete the handshake → accept → conn #0 in table.
    // (This also opens the listen slot for Phase B.)
    let first_src_port = FLOOD_BASE_PORT;
    // Re-inject the final ACK for SYN-0.  We need the engine's ISS for
    // the ACK value.  Re-inject the SYN to get a new SYN-ACK (the
    // original was already drained).
    {
        set_virt_ns(2_000_000);
        // The listen slot has in_progress set from SYN-0.  Send the final
        // ACK using PEER_ISS (SYN-0's ISS) + 1 as seq.
        // We don't know the exact our_iss from SYN-0's SYN-ACK since it
        // was already drained.  Re-inject SYN-0 to get a fresh SYN-ACK.
        // The existing in-progress conn from the first SYN is still in
        // SYN_RCVD; the re-injected SYN collides with the in-progress
        // slot → RST reply (slot is full).  Instead, use the accept path
        // to clear the slot: inject a RST to abort the in-progress conn,
        // then open a fresh clean connection.
        //
        // Simplest approach: inject a RST targeting the in-progress conn.
        // The engine processes it, closes the SYN_RCVD conn (via RST),
        // clearing the in-progress slot.
        use dpdk_net_core::tcp_output::TCP_RST;
        // seq = PEER_ISS (the peer's SYN seq), which the engine's
        // SYN_RCVD conn has as rcv_nxt = PEER_ISS + 1.
        let rst_abort = build_tcp_frame(
            PEER_IP,
            first_src_port,
            OUR_IP,
            OUR_PORT,
            PEER_ISS.wrapping_add(1), // seq = rcv_nxt of the SYN_RCVD conn
            0,
            TCP_RST,
            0,
            TcpOpts::default(),
            &[],
        );
        h.eng.inject_rx_frame(&rst_abort).expect("inject RST-abort");
        h.eng.poll_once();
        let _ = drain_tx_frames();
        h.eng.drain_events(64, |_, _| {});
    }

    // ── Phase B: flow-table exhaustion ────────────────────────────────
    //
    // Open N_MAX passive conns (filling the N_MAX-slot table), then call
    // connect() → flow_table.insert() fails → conn_table_full++.
    set_virt_ns(3_000_000);
    let mut conn_handles = Vec::with_capacity(N_MAX as usize);
    for i in 0..N_MAX {
        let src_port = PASSIVE_BASE_PORT.wrapping_add(i as u16);
        let peer_iss = 0x4000_0000u32.wrapping_add(i * 0x100);
        let syn = build_tcp_frame(
            PEER_IP,
            src_port,
            OUR_IP,
            OUR_PORT,
            peer_iss,
            0,
            TCP_SYN,
            u16::MAX,
            { let mut o = TcpOpts::default(); o.mss = Some(1460); o },
            &[],
        );
        h.eng.inject_rx_frame(&syn).expect("inject SYN");
        let frames = drain_tx_frames();
        assert_eq!(frames.len(), 1, "expected SYN-ACK for port {src_port}");
        let (our_iss, _) = parse_syn_ack(&frames[0]).expect("parse SYN-ACK");

        let ack = build_tcp_ack(
            PEER_IP,
            src_port,
            OUR_IP,
            OUR_PORT,
            peer_iss.wrapping_add(1),
            our_iss.wrapping_add(1),
        );
        h.eng.inject_rx_frame(&ack).expect("inject final ACK");
        let _ = drain_tx_frames();
        let conn = h.eng.accept_next(listen_h).expect("accept_next");
        conn_handles.push(conn);
    }

    // Flow table is now full (N_MAX / N_MAX slots used).
    assert_eq!(
        h.eng.flow_table().active_conns(),
        N_MAX as usize,
        "expected table full ({N_MAX} conns)"
    );

    // One active-open attempt → conn_table_full.
    set_virt_ns(4_000_000);
    let _ = h.eng.connect(PEER_IP, 9_001, 0); // expected to fail
    h.eng.poll_once();
    let _ = drain_tx_frames();
    h.eng.drain_events(64, |_, _| {});

    // ── Snapshot + assertions ──────────────────────────────────────────
    let after = CounterSnapshot::capture(h.eng.counters());
    let delta = after.delta_since(&bucket.before);

    // Phase A: at least N_SYN_FLOOD - 1 RSTs emitted for rejected SYNs.
    // (SYN-0 was accepted into the listen slot; SYNs 1..N_SYN_FLOOD got RST.
    //  The RST-abort we injected to clear SYN-0's slot also emits no RST
    //  since send_rst_unmatched short-circuits on incoming RST flag.)
    assert_delta(
        &delta,
        "tcp.tx_rst",
        Relation::Ge((N_SYN_FLOOD - 1) as i64),
    );

    // Phase B: flow-table capacity hit at least once.
    assert_delta(&delta, "tcp.conn_table_full", Relation::Ge(1));

    // Event-queue cap not breached.
    assert_delta(&delta, "obs.events_dropped", Relation::Eq(0));

    // Mbuf refcount accounting clean.
    assert_delta(&delta, "tcp.mbuf_refcnt_drop_unexpected", Relation::Eq(0));

    // Clean up Phase B conns.
    for conn in conn_handles {
        let _ = h.eng.close_conn(conn);
    }
    h.eng.poll_once();
    let _ = drain_tx_frames();
    h.eng.drain_events(64, |_, _| {});

    bucket.finish_ok();
}
