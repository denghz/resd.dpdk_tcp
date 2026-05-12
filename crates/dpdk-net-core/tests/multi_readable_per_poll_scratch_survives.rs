//! Regression: multi-`deliver_readable`-per-poll must NOT clobber earlier
//! events' iovec backing.
//!
//! # Root-cause analysis
//!
//! Pre-fix, `Engine::deliver_readable` cleared the per-conn
//! `readable_scratch_iovecs` Vec at the top of every invocation before
//! pushing the new event's iovecs. When two TCP data segments for the
//! same conn arrived inside one RX burst (or were back-to-back-injected
//! between two `poll_once` calls), the SECOND `deliver_readable` call
//! cleared the FIRST event's scratch entries before pushing its own.
//! Both events landed in the `EventQueue` with valid `total_len` /
//! `seg_count` payloads, but only the LAST event's bytes were
//! accessible through the per-conn scratch — the first event's
//! `seg_idx_start..seg_idx_start+seg_count` range now indexed into
//! cleared scratch slots whose `base` pointers had also dropped their
//! mbuf refcount (`delivered_segments.clear()` at the top of
//! `deliver_readable`).
//!
//! The bench-rx-burst surface symptom was misaligned-parse
//! `peer_send_ns ≈ 0` records (zero-pad bytes parsed as a header),
//! producing `latency_ns = dut_recv_ns - 0 ≈ 1.78e18 ns` percentile
//! outliers (see commit `25f5353` for the `PEER_SEND_NS_FLOOR`
//! sentinel workaround in `tools/bench-rx-burst`).
//!
//! # Fix
//!
//! `deliver_readable` now APPENDS its newly-emitted iovec entries to
//! the per-conn scratch instead of clearing it. The per-event slice is
//! recorded as `seg_idx_start = scratch.len() before push` and
//! `seg_count = number of entries pushed in this emit`. The scratch
//! is cleared only at the top of `poll_once` (and `delivered_segments`
//! is migrated to the same accumulating model so the mbuf refcounts
//! holding the iovec `base` pointers live across multiple emits in
//! one poll).
//!
//! # What this test verifies
//!
//! 1. Two consecutive `inject_rx_frame` calls deliver two distinct
//!    payloads to the same conn, each firing a `deliver_readable`
//!    call (no `poll_once` between).
//! 2. The `EventQueue` carries TWO `Readable` events, one per payload.
//! 3. Reading bytes via each event's `seg_idx_start..seg_idx_start +
//!    seg_count` range against `readable_scratch_iovecs` must yield
//!    THAT event's payload bytes intact — even though the second
//!    `deliver_readable` ran first-event-emit-then-scratch-grow.
//!
//! Pre-fix this fails: the first event's reconstructed bytes either
//! point into cleared slots (panic on slice bounds), or read garbage,
//! or read the second event's bytes (depending on Vec growth pattern).
//!
//! Post-fix it passes: the first event's slice covers its own payload;
//! the second event's slice covers its own payload; the two payloads
//! are byte-distinct and decode cleanly.

#![cfg(feature = "test-server")]

mod common;

use dpdk_net_core::clock::set_virt_ns;
use dpdk_net_core::engine::{eal_init, Engine};
use dpdk_net_core::tcp_events::InternalEvent;
use dpdk_net_core::tcp_options::TcpOpts;
use dpdk_net_core::tcp_output::{TCP_ACK, TCP_PSH};

const PEER_PORT: u16 = 40_001;
const OUR_PORT: u16 = 5556;

#[test]
fn two_readable_events_in_one_poll_keep_their_iovecs_intact() {
    // ----- Setup: 3WHS to ESTABLISHED via test-server bypass. -----
    set_virt_ns(0);
    eal_init(&common::test_eal_args()).expect("eal_init");
    let eng = Engine::new(common::test_server_config()).expect("Engine::new");
    let listen_h = eng.listen(common::OUR_IP, OUR_PORT).expect("listen");

    // Drain any stray TX from previous scenarios in this process.
    let _ = dpdk_net_core::test_tx_intercept::drain_tx_frames();

    // SYN
    set_virt_ns(1_000_000);
    let peer_iss: u32 = 0x20000000;
    let syn = common::build_tcp_syn(common::PEER_IP, PEER_PORT, common::OUR_IP, OUR_PORT, peer_iss, 1460);
    eng.inject_rx_frame(&syn).expect("inject SYN");
    let frames = dpdk_net_core::test_tx_intercept::drain_tx_frames();
    assert_eq!(frames.len(), 1, "exactly one SYN-ACK expected");
    let (our_iss, _ack) = common::parse_syn_ack(&frames[0]).expect("parse SYN-ACK");

    // Final ACK
    set_virt_ns(2_000_000);
    let final_ack = common::build_tcp_ack(
        common::PEER_IP, PEER_PORT, common::OUR_IP, OUR_PORT,
        peer_iss.wrapping_add(1),
        our_iss.wrapping_add(1),
    );
    eng.inject_rx_frame(&final_ack).expect("inject final ACK");
    let _ = dpdk_net_core::test_tx_intercept::drain_tx_frames();
    let conn = eng.accept_next(listen_h).expect("accept_next yields conn");

    // ----- Drain any Readable events from the handshake (defensive). -----
    let mut events_drained = 0u32;
    eng.drain_events(64, |_ev, _| { events_drained += 1; });
    let _ = events_drained;

    // ----- Two distinct payloads, injected back-to-back without poll_once. -----
    // Payload A is all 0xAA; payload B is all 0xBB. The bytes are
    // chosen to be trivially distinguishable so a UAF or scratch-
    // overwrite produces an obvious assertion failure (mixed A/B
    // bytes in the reconstructed buffer for the first event).
    const PAYLOAD_LEN: usize = 64;
    let payload_a: Vec<u8> = vec![0xAA; PAYLOAD_LEN];
    let payload_b: Vec<u8> = vec![0xBB; PAYLOAD_LEN];

    set_virt_ns(3_000_000);
    let mut peer_seq = peer_iss.wrapping_add(1);

    // Segment 1: payload A.
    let seg1 = common::build_tcp_frame(
        common::PEER_IP, PEER_PORT, common::OUR_IP, OUR_PORT,
        peer_seq, our_iss.wrapping_add(1),
        TCP_ACK | TCP_PSH, u16::MAX, TcpOpts::default(), &payload_a,
    );
    eng.inject_rx_frame(&seg1).expect("inject payload A");
    peer_seq = peer_seq.wrapping_add(payload_a.len() as u32);

    // Segment 2: payload B. No poll_once between — this is the
    // multi-deliver-per-poll scenario exactly.
    let seg2 = common::build_tcp_frame(
        common::PEER_IP, PEER_PORT, common::OUR_IP, OUR_PORT,
        peer_seq, our_iss.wrapping_add(1),
        TCP_ACK | TCP_PSH, u16::MAX, TcpOpts::default(), &payload_b,
    );
    eng.inject_rx_frame(&seg2).expect("inject payload B");

    // ----- Drain the Readable events and reconstruct each one's bytes. -----
    let mut readable_views: Vec<(u32, u32, u32)> = Vec::new(); // (seg_idx_start, seg_count, total_len)
    let mut reconstructed: Vec<Vec<u8>> = Vec::new();

    // Pop events ourselves so we keep the iovec read coupled with the
    // event metadata (mirrors how `dpdk_net_poll` materializes the ABI
    // payload, and how `bench-rx-burst`'s drain works).
    loop {
        let ev = {
            let mut q = eng.events();
            q.pop()
        };
        let Some(ev) = ev else { break };
        if let InternalEvent::Readable { conn: ch, seg_idx_start, seg_count, total_len, .. } = ev {
            if ch == conn {
                readable_views.push((seg_idx_start, seg_count, total_len));
                let ft = eng.flow_table();
                let c = ft.get(conn).expect("conn must still exist");
                let start = seg_idx_start as usize;
                let end = start + seg_count as usize;
                // The slice MUST be in-bounds — pre-fix, the first event's
                // (start, end) would index into a scratch that was cleared
                // by the second emit; if Vec grew (push past prior len) the
                // slice could be entirely beyond the current Vec len.
                let scratch_len = c.readable_scratch_iovecs.len();
                assert!(
                    end <= scratch_len,
                    "event slice [{start}..{end}] out of scratch bounds {scratch_len} \
                     — multi-deliver-per-poll UAF regression",
                );
                let mut bytes = Vec::<u8>::with_capacity(total_len as usize);
                for iv in &c.readable_scratch_iovecs[start..end] {
                    // SAFETY: scratch entries reference live mbuf payloads;
                    // mbufs are pinned by the conn's `delivered_segments`
                    // (which under the fix also accumulates across emits in
                    // the same poll, so prior-event refcounts are still held).
                    let slice = unsafe {
                        std::slice::from_raw_parts(iv.base, iv.len as usize)
                    };
                    bytes.extend_from_slice(slice);
                }
                reconstructed.push(bytes);
            }
        }
    }

    // ----- Assertions. -----
    assert_eq!(
        readable_views.len(), 2,
        "expected exactly two Readable events; got {}: {:?}",
        readable_views.len(), readable_views,
    );

    // total_len's must match the injected payloads.
    assert_eq!(readable_views[0].2 as usize, payload_a.len(),
        "event 1 total_len = {}, want {}", readable_views[0].2, payload_a.len());
    assert_eq!(readable_views[1].2 as usize, payload_b.len(),
        "event 2 total_len = {}, want {}", readable_views[1].2, payload_b.len());

    // Critical assertion: each event's reconstructed bytes must match
    // its OWN payload, not the other one. Pre-fix, both events'
    // reconstructed slices would read from the same scratch range
    // (which held only the LAST emit's iovec), producing two copies
    // of payload B or out-of-bounds reads.
    assert_eq!(
        reconstructed[0], payload_a,
        "event 1 reconstructed bytes mismatched payload A — \
         multi-deliver-per-poll iovec clobber regression",
    );
    assert_eq!(
        reconstructed[1], payload_b,
        "event 2 reconstructed bytes mismatched payload B — \
         multi-deliver-per-poll iovec clobber regression",
    );

    // Cleanup: release pinned RX mbufs before the engine drops its
    // mempool. Mirrors `CovHarness::Drop` and `tests/common::CovHarness`.
    eng.test_clear_pinned_rx_mbufs();
}
