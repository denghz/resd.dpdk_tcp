//! bench-micro::throughput — sustained-rate benches for the 4 real-code families.
//!
//! Latency benches (`poll.rs`, `flow_lookup.rs`, `tcp_input.rs`, `timer.rs`)
//! measure single-call cost. This file measures sustained ops/sec under
//! continuous load — surfaces allocation patterns, cache thrash, and
//! drift over time that latency benches don't catch.
//!
//! Each bench batches K operations per criterion iteration. Reports
//! ops/sec via `criterion::Throughput::Elements`. measurement_time
//! defaults to 30s to surface drift; sample_size scaled accordingly.

use criterion::{black_box, criterion_group, criterion_main, Criterion, Throughput};
use dpdk_net_core::engine::test_support::EngineNoEalHarness;
use dpdk_net_core::engine::DEFAULT_RTT_HISTOGRAM_EDGES_US;
use dpdk_net_core::flow_table::{FlowTable, FourTuple};
use dpdk_net_core::tcp_conn::TcpConn;
use dpdk_net_core::tcp_input::{dispatch, MbufInsertCtx, ParsedSegment};
use dpdk_net_core::tcp_options::TcpOpts;
use dpdk_net_core::tcp_output::TCP_ACK;
use dpdk_net_core::tcp_state::TcpState;
use std::time::{Duration, Instant};

const BATCH: u64 = 1024;
const TEST_SEND_BUF_BYTES: u32 = 256 * 1024;

// =====================================================================
// poll throughput — sustained EngineNoEalHarness::poll_once iterations
// =====================================================================

fn bench_poll_throughput_empty(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput");
    group
        .throughput(Throughput::Elements(BATCH))
        .measurement_time(Duration::from_secs(30))
        .sample_size(50);

    group.bench_function("poll_empty_throughput", |b| {
        let mut h = EngineNoEalHarness::new(64, 1_000_000);
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                for _ in 0..BATCH {
                    h.poll_once();
                    black_box(&h);
                }
            }
            start.elapsed()
        });
    });

    group.finish();
}

fn bench_poll_throughput_with_timers(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput");
    group
        .throughput(Throughput::Elements(BATCH))
        .measurement_time(Duration::from_secs(30))
        .sample_size(50);

    group.bench_function("poll_idle_with_timers_throughput", |b| {
        let mut h = EngineNoEalHarness::new(64, 1_000_000);
        // Pre-populate 256 non-firing timers so advance walks a real
        // bucket chain on every iter.
        let _ids = h.pre_populate_timers(256, u64::MAX / 2);
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                for _ in 0..BATCH {
                    h.poll_once();
                    black_box(&h);
                }
            }
            start.elapsed()
        });
    });

    group.finish();
}

// =====================================================================
// flow_lookup throughput — sustained 4-tuple lookups, varying flow count
// =====================================================================

fn bench_flow_lookup_throughput_hot(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput");
    group
        .throughput(Throughput::Elements(BATCH))
        .measurement_time(Duration::from_secs(30))
        .sample_size(50);

    let (ft, target) = build_flow_table_for_bench(16);

    group.bench_function("flow_lookup_hot_throughput", |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                for _ in 0..BATCH {
                    let h = ft.lookup_by_tuple(black_box(&target));
                    black_box(h);
                }
            }
            start.elapsed()
        });
    });

    group.finish();
}

// Helper: build a populated FlowTable + return one tuple for hot-lookup.
fn build_flow_table_for_bench(n_entries: usize) -> (FlowTable, FourTuple) {
    let mut ft = FlowTable::new(64);
    let mut target = None;
    for i in 0..n_entries {
        let t = FourTuple {
            local_ip: 0x0a_00_00_02,
            local_port: 40_000 + i as u16,
            peer_ip: 0x0a_00_00_01,
            peer_port: 5_000 + i as u16,
        };
        let conn =
            TcpConn::new_client(t, 1_000 + i as u32, 1460, 1024, 2048, 5000, 5000, 1_000_000);
        ft.insert(conn).expect("slot available");
        if i == 0 {
            target = Some(t);
        }
    }
    (ft, target.expect("at least one entry"))
}

// =====================================================================
// timer throughput — sustained add+cancel round-trips
// =====================================================================

fn bench_timer_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput");
    group
        .throughput(Throughput::Elements(BATCH))
        .measurement_time(Duration::from_secs(30))
        .sample_size(50);

    group.bench_function("timer_add_cancel_throughput", |b| {
        let mut h = EngineNoEalHarness::new(64, 1_000_000);
        // T7.7 H1: walk a `now_ns` cursor forward each inner iter and
        // call `timer_wheel.advance` after the cancel so the just-
        // cancelled slot is recycled to `free_list`. The pre-T7.7
        // bench called only `add` + `cancel` and never `advance`, so
        // every `add` pushed a new entry onto `slots` / `generations`
        // (uncoded, the cancellation only sets `cancelled=true` and
        // doesn't touch the slot Vec). Across the bench's ~28 Mops/s
        // × 30s sample window that grew the slots Vec to ~10⁹
        // entries, triggering kernel page-fault traffic that
        // dominated the post-baseline TBP at ~34% of profile time
        // (clear_page_rep + do_user_addr_fault + zap_pte_range +
        // do_anonymous_page + __handle_mm_fault + mem_cgroup_*).
        //
        // The real engine calls `advance` every `poll_once`, so the
        // post-fix shape (add → cancel → advance) better mirrors the
        // production pattern: cancellation is followed by the next
        // tick's wheel advance which sweeps the cancelled slot back
        // to `free_list` for reuse on the next `add`.
        const FIRE_DELAY_NS: u64 = dpdk_net_core::tcp_timer_wheel::TICK_NS;
        let mut cursor_ns: u64 = 0;
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                for _ in 0..BATCH {
                    let id = h.timer_add(
                        black_box(cursor_ns + FIRE_DELAY_NS),
                        black_box(0),
                    );
                    let _cancelled = h.timer_cancel(id);
                    cursor_ns += FIRE_DELAY_NS;
                    let _fired = h.timer_wheel.advance(cursor_ns);
                    black_box(&h);
                }
            }
            start.elapsed()
        });
    });

    group.finish();
}

// =====================================================================
// tcp_input throughput — sustained dispatch through the segment-handling
// path. Setup pattern mirrors `tcp_input.rs::bench_tcp_input_data_segment`,
// but per-iter setup is lifted outside the closure so each batched op
// is just the dispatch call.
// =====================================================================

/// Shared ESTABLISHED-state conn construction. Mirrors the in-tree
/// `tcp_input::tests::est_conn` shape (see tcp_input.rs bench file).
fn make_est_conn(iss: u32, irs: u32, peer_wnd: u16, ts: Option<u32>, sack: bool) -> TcpConn {
    let t = FourTuple {
        local_ip: 0x0a_00_00_02,
        local_port: 40000,
        peer_ip: 0x0a_00_00_01,
        peer_port: 5000,
    };
    let mut c = TcpConn::new_client(t, iss, 1460, 1024, 2048, 5000, 5000, 1_000_000);
    c.state = TcpState::Established;
    c.snd_una = iss.wrapping_add(1);
    c.snd_nxt = iss.wrapping_add(1);
    c.irs = irs;
    c.rcv_nxt = irs.wrapping_add(1);
    c.snd_wnd = peer_wnd as u32;
    if let Some(ts_recent) = ts {
        c.ts_enabled = true;
        c.ts_recent = ts_recent;
    }
    c.sack_enabled = sack;
    c
}

fn bench_tcp_input_throughput_data(c: &mut Criterion) {
    let mut group = c.benchmark_group("throughput");
    group
        .throughput(Throughput::Elements(BATCH))
        .measurement_time(Duration::from_secs(30))
        .sample_size(50);

    group.bench_function("tcp_input_data_throughput", |b| {
        // Encode a minimal Timestamps option. SACK_PERMITTED was already
        // exchanged on the handshake; we don't repeat it per-segment.
        let peer_opts = TcpOpts {
            timestamps: Some((0x0000_1000, 0xCAFEBABE)),
            ..Default::default()
        };
        let mut opts_buf = [0u8; 40];
        let opts_len = peer_opts.encode(&mut opts_buf).expect("encode opts");
        let payload = [0xABu8; 64];

        // Fake mbuf storage for the in-order append path's refcount shim.
        // The shim only touches the first cacheline's `refcnt` field.
        let mut fake_storage: Box<[u8; 256]> = Box::new([0u8; 256]);
        let mbuf_ctx = MbufInsertCtx {
            mbuf: unsafe {
                std::ptr::NonNull::new_unchecked(
                    fake_storage.as_mut_ptr() as *mut dpdk_net_sys::rte_mbuf
                )
            },
            payload_offset: 54,
        };

        // T7.7 H1: hoist conn construction out of the timed inner loop.
        // Pre-T7.7 TBP showed 36% of CPU time in libc malloc/free from
        // `make_est_conn` rebuilding TcpConn (≈5 heap allocations:
        // SendQueue VecDeque(256 KiB), RecvQueue, ReorderQueue, Vec
        // timer_ids(32), SendRetrans deque) per inner iter, with the
        // matching `drop_in_place::TcpConn` adding 3.2%. That harness
        // overhead masked the actual dispatch hotspots TBP could resolve.
        //
        // The bench's intent was always "sustained dispatch throughput",
        // not "dispatch + per-call alloc-free". This restructure builds
        // one conn outside the timed window and resets only the seq /
        // window-state fields dispatch mutates between BATCH calls. The
        // reset must keep `seg.seq == rcv_nxt` true so each dispatch
        // takes the in-order data path; everything else returns to the
        // post-`make_est_conn` shape.
        //
        // Fields dispatch mutates that need per-iter reset (verified by
        // reading `handle_established` for the in-order, TS-only,
        // SACK-permitted, no-new-ACK case the bench exercises):
        //   - `rcv_nxt`        ← advanced by payload.len()
        //   - `snd_wnd`        ← seg.window (65535)
        //   - `snd_wl1` / `snd_wl2`
        //   - `recv.bytes`     ← one InOrderSegment pushed
        //   - `last_advertised_wnd`
        //   - `ts_recent`      ← seg's tsval (0x1000)
        //   - `ts_recent_age`  ← clock::now_ns()
        //   - `last_sack_trigger` (cleared on in-order)
        // `snd_una`, `snd_nxt`, `snd_retrans`, `sack_scoreboard`,
        // `timer_ids`, `rtt_est` are not changed in this code path
        // (seg.ack == snd_una, no new bytes acked, in-order, etc.).
        let mut conn = make_est_conn(1000, 5000, 1024, Some(200), true);

        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                for _ in 0..BATCH {
                    // Reset only the fields dispatch mutates so the next
                    // call still sees seq=5001 as the in-order point.
                    conn.rcv_nxt = 5001;
                    conn.snd_wnd = 1024;
                    conn.snd_wl1 = 0;
                    conn.snd_wl2 = 0;
                    conn.last_advertised_wnd = None;
                    conn.last_sack_trigger = None;
                    conn.ts_recent = 200;
                    conn.ts_recent_age = 0;
                    // Drop accumulated in-order segments without
                    // dropping the VecDeque's backing capacity.
                    conn.recv.bytes.clear();

                    let seg = ParsedSegment {
                        src_port: 5000,
                        dst_port: 40000,
                        seq: 5001,
                        ack: 1001,
                        flags: TCP_ACK,
                        window: 65535,
                        header_len: 20 + opts_len,
                        payload: &payload,
                        options: &opts_buf[..opts_len],
                    };
                    let out = dispatch(
                        black_box(&mut conn),
                        black_box(&seg),
                        &DEFAULT_RTT_HISTOGRAM_EDGES_US,
                        TEST_SEND_BUF_BYTES,
                        Some(mbuf_ctx),
                    );
                    black_box(out);
                }
            }
            start.elapsed()
        });

        // Keep fake_storage alive until after the bench function returns.
        let _ = &mut fake_storage;
    });

    group.finish();
}

criterion_group! {
    name = benches;
    config = Criterion::default();
    targets =
        bench_poll_throughput_empty,
        bench_poll_throughput_with_timers,
        bench_flow_lookup_throughput_hot,
        bench_timer_throughput,
        bench_tcp_input_throughput_data
}
criterion_main!(benches);
