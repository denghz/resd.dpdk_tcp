//! Pressure Suite — `pressure-sack-blocks-hermeticity`.
//! A11.3 Lane B.
//!
//! Workload: open one SACK-enabled connection via the test-server bypass.
//! Two stimulus paths run in sequence:
//!
//! **Path A — emit SACKs (engine as receiver):**
//! Inject 8 OOO data frames, each creating a new gap in the engine's RX
//! sequence space.  The engine responds to each OOO arrival with an ACK
//! carrying SACK blocks that report the buffered-but-not-yet-delivered
//! ranges.  This exercises the SACK-emission path.
//!
//! **Path B — decode SACKs (engine as sender):**
//! Inject 4 ACK frames from the peer, each carrying 4 SACK blocks (the
//! `MAX_SACK_BLOCKS_DECODE` limit), referencing 16 distinct hole ranges
//! (= 4× the `MAX_SACK_BLOCKS_EMIT = 3` emission cap, and 4× the
//! `MAX_SACK_BLOCKS_DECODE = 4` decode cap).  This exercises the receive-
//! side SACK-option decode path at its limits.
//!
//! Engine config:
//!   * `tcp_timestamps = false` — simplifies option encoding; SACK options
//!     in Path B do not need a TS companion.
//!   * `tcp_sack = true` — SACK negotiated during the SYN/SYN-ACK.
//!   * `max_connections = 4`, `recv_buffer_bytes = 256 KiB` (default).
//!
//! Counters asserted (deltas across both paths):
//!   * `tcp.tx_sack_blocks` > 0  — engine emitted at least one SACK block
//!       in its ACKs after OOO arrival (Path A).
//!   * `tcp.rx_bad_option` == 0  — all injected SACK options were parsed
//!       correctly (Path B hermeticity).
//!   * `obs.events_dropped` == 0  — event-queue cap not breached.
//!   * `tcp.mbuf_refcnt_drop_unexpected` == 0  — refcount accounting clean.
//!
//! Gated behind `all(feature = "pressure-test", feature = "test-server")`.

#![cfg(all(feature = "pressure-test", feature = "test-server"))]

mod common;

use common::pressure::{assert_delta, CounterSnapshot, PressureBucket, Relation};
use common::{build_tcp_frame, OUR_IP, PEER_IP};
use dpdk_net_core::engine::EngineConfig;
use dpdk_net_core::tcp_options::{SackBlock, TcpOpts};
use dpdk_net_core::tcp_output::{TCP_ACK, TCP_PSH, TCP_SYN};

/// Source port for the synthetic peer (reused across all injections).
const PEER_PORT: u16 = 40_000;
/// Listening port for the engine's passive open.
const OUR_PORT: u16 = 5555;
/// Peer's initial sequence number for the handshake.
const PEER_ISS: u32 = 0x2000_0000;
/// Number of OOO data frames to inject in Path A.
const OOO_FRAMES: u32 = 8;
/// Payload bytes per OOO frame.  Small enough to fit in a single mbuf but
/// large enough to create clearly distinct gap ranges.
const OOO_SEG: u32 = 256;
/// ACK frames with 4 SACK blocks each to inject in Path B.
const SACK_ACK_FRAMES: u32 = 4;

#[test]
fn pressure_sack_blocks_hermeticity() {
    use common::CovHarness;
    use dpdk_net_core::clock::set_virt_ns;
    use dpdk_net_core::test_tx_intercept::drain_tx_frames;

    // Disable timestamps so Path-B ACKs don't need a timestamp companion.
    let mut h = CovHarness::new_with_config(EngineConfig {
        port_id: u16::MAX,
        local_ip: OUR_IP,
        gateway_ip: PEER_IP,
        gateway_mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x02],
        tcp_mss: 1460,
        max_connections: 4,
        tcp_msl_ms: 100,
        tcp_timestamps: false,
        tcp_sack: true,
        ..Default::default()
    });

    let bucket = PressureBucket::open(
        "pressure-sack-blocks",
        "hermeticity",
        h.eng.counters(),
    );

    // ── Handshake: negotiate SACK ──────────────────────────────────────
    //
    // Build a SYN with `sack_permitted = true` so the engine records
    // `conn.sack_permitted = true` and emits SACK blocks in its ACKs.
    // Manually replay the 3-way handshake (we can't use do_passive_open
    // because its build_tcp_syn only sets the MSS option).
    set_virt_ns(1_000_000);
    let listen_h = h.eng.listen(OUR_IP, OUR_PORT).expect("listen");
    let _ = drain_tx_frames();

    let syn = {
        let mut opts = TcpOpts::default();
        opts.mss = Some(1460);
        opts.sack_permitted = true;
        build_tcp_frame(
            PEER_IP, PEER_PORT, OUR_IP, OUR_PORT,
            PEER_ISS, 0,
            TCP_SYN, u16::MAX,
            opts, &[],
        )
    };
    h.eng.inject_rx_frame(&syn).expect("inject SYN");
    let syn_ack_frames = drain_tx_frames();
    assert_eq!(syn_ack_frames.len(), 1, "exactly one SYN-ACK expected");

    // Parse the SYN-ACK to extract our ISS and peer's ack value.
    let syn_ack = &syn_ack_frames[0];
    let (our_iss, _ack) = dpdk_net_core::test_server::test_packet::parse_syn_ack(syn_ack)
        .expect("parse SYN-ACK");

    set_virt_ns(2_000_000);
    let final_ack = {
        build_tcp_frame(
            PEER_IP, PEER_PORT, OUR_IP, OUR_PORT,
            PEER_ISS.wrapping_add(1), our_iss.wrapping_add(1),
            TCP_ACK, u16::MAX, TcpOpts::default(), &[],
        )
    };
    h.eng.inject_rx_frame(&final_ack).expect("inject final ACK");
    let _ = drain_tx_frames();

    let conn = h.eng.accept_next(listen_h).expect("accept_next");

    // Populate CovHarness tracking fields so helpers and Drop are consistent.
    h.our_iss.set(our_iss);
    h.peer_seq.set(PEER_ISS.wrapping_add(1));

    // rcv_nxt after handshake = PEER_ISS + 1.
    let rcv_nxt = PEER_ISS.wrapping_add(1);

    // ── Path A: OOO frames → engine emits SACKs ────────────────────────
    //
    // Inject OOO_FRAMES frames, each OOO_SEG bytes, interleaved with
    // 2×OOO_SEG gaps to create distinct SACK ranges.  Example with
    // OOO_SEG=256:
    //
    //   OOO-0: seq = rcv_nxt + 256         [256,  512)
    //   OOO-1: seq = rcv_nxt + 768         [768, 1024)
    //   OOO-2: seq = rcv_nxt + 1280        [1280, 1536)
    //   ...
    //
    // Each frame causes the engine to (a) queue the OOO bytes and (b)
    // emit an ACK with SACK blocks covering the OOO ranges → tx_sack_blocks++.
    set_virt_ns(3_000_000);
    for i in 0..OOO_FRAMES {
        // Skip one OOO_SEG unit between frames to create independent gaps.
        let ooo_seq = rcv_nxt.wrapping_add(OOO_SEG + i * 2 * OOO_SEG);
        let frame = build_tcp_frame(
            PEER_IP, PEER_PORT, OUR_IP, OUR_PORT,
            ooo_seq, our_iss.wrapping_add(1),
            TCP_ACK | TCP_PSH, u16::MAX,
            TcpOpts::default(),
            &vec![0x5au8; OOO_SEG as usize],
        );
        h.eng.inject_rx_frame(&frame).expect("inject OOO");
        h.eng.poll_once();
        let _ = drain_tx_frames();
        h.eng.drain_events(64, |_, _| {});
    }

    // ── Path B: peer ACKs with 4 SACK blocks each ─────────────────────
    //
    // Inject SACK_ACK_FRAMES (=4) ACK frames from the peer.  Each carries
    // 4 SACK blocks (MAX_SACK_BLOCKS_DECODE = 4), referencing ranges of
    // the engine's sent sequence space.  We reference a sequence window
    // beyond our ISS to simulate the peer telling us "I received these
    // ranges out of order".
    //
    // Total SACK holes referenced: SACK_ACK_FRAMES × 4 = 16.
    //
    // The engine decodes each frame's options and (if SACK is permitted)
    // updates its retransmit-queue sacked flags.  The hermeticity claim:
    // no rx_bad_option is generated and the engine does not panic.
    set_virt_ns(4_000_000);
    let snd_una = our_iss.wrapping_add(1);
    for round in 0..SACK_ACK_FRAMES {
        // Compute 4 non-overlapping SACK block ranges in the engine's
        // send sequence space (snd_una + offset).  We offset by enough
        // that consecutive rounds don't collide.
        let base = snd_una.wrapping_add(1 + round * 4 * 512);
        let mut opts = TcpOpts::default();
        for j in 0u32..4 {
            let left  = base.wrapping_add(j * 512);
            let right = left.wrapping_add(256);
            // push_sack_block_decode allows up to 4 blocks (decode cap).
            opts.push_sack_block_decode(SackBlock { left, right });
        }
        let frame = build_tcp_frame(
            PEER_IP, PEER_PORT, OUR_IP, OUR_PORT,
            PEER_ISS.wrapping_add(1), our_iss.wrapping_add(1),
            TCP_ACK, u16::MAX, opts, &[],
        );
        h.eng.inject_rx_frame(&frame).expect("inject SACK ACK");
        h.eng.poll_once();
        let _ = drain_tx_frames();
        h.eng.drain_events(64, |_, _| {});
    }

    // ── Settle ─────────────────────────────────────────────────────────
    h.eng.poll_once();
    let _ = drain_tx_frames();
    h.eng.drain_events(64, |_, _| {});

    // ── Snapshot + assertions ──────────────────────────────────────────
    let after = CounterSnapshot::capture(h.eng.counters());
    let delta = after.delta_since(&bucket.before);

    // Path A: engine emitted SACK blocks for the OOO arrivals.
    assert_delta(&delta, "tcp.tx_sack_blocks", Relation::Gt(0));

    // Path B hermeticity: all SACK options were parsed without error.
    assert_delta(&delta, "tcp.rx_bad_option", Relation::Eq(0));

    // Event-queue cap not breached.
    assert_delta(&delta, "obs.events_dropped", Relation::Eq(0));

    // Mbuf refcount accounting clean throughout.
    assert_delta(&delta, "tcp.mbuf_refcnt_drop_unexpected", Relation::Eq(0));

    // Close connection.
    let _ = h.eng.close_conn(conn);
    h.eng.poll_once();
    let _ = drain_tx_frames();
    h.eng.drain_events(64, |_, _| {});

    bucket.finish_ok();
}
