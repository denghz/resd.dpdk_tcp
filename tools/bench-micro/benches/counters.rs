//! bench-micro::counters — spec §11.2 target 12.
//!
//! Two targets:
//!
//! - `bench_counters_one_per_group` reads ONE representative atomic per
//!   counter group (6 atomics: eth, ip, tcp, poll, obs, fault_injector).
//!   Surfaces the fixed per-load atomic cost without amortizing it across
//!   the full snapshot. This is what the older `bench_counters_read`
//!   measured; it is renamed for honesty — the original name implied a
//!   whole-struct snapshot which it did not perform.
//!
//! - `bench_counters_full_snapshot` reads EVERY public atomic field
//!   across all six counter sub-structs (~245 atomics including the
//!   `tcp.state_trans[11][11]` matrix). This is what
//!   `dpdk_net_counters_t` snapshot consumers actually pay when they
//!   walk the whole struct — the original §11.2-target intent.
//!
//! # Stubbing note
//!
//! `dpdk_net_counters(engine)` returns a `*const dpdk_net_counters_t`
//! pointing at the Engine's owned `Counters`. Without a live Engine we
//! construct a standalone `Counters` directly and measure the read
//! cost. The FFI entry adds only pointer validation (benchmarked
//! elsewhere) so this proxy matches the spec's "read of all counter
//! groups" intent.
//!
//! # Field-count drift
//!
//! `bench_counters_full_snapshot` hand-enumerates every public atomic
//! field in `crates/dpdk-net-core/src/counters.rs`. If a new
//! `AtomicU64` or `AtomicU32` field is added there without also being
//! added below, the bench will still compile and run — the count will
//! just be stale. Refresh both sites together; the
//! `KNOWN_COUNTER_COUNT` compile-time pin in `counters.rs` is the
//! authoritative drift detector but only tracks names listed in
//! `ALL_COUNTER_NAMES`, not raw field counts.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use dpdk_net_core::counters::Counters;
use std::sync::atomic::Ordering;
use std::time::Duration;

fn bench_counters_one_per_group(c: &mut Criterion) {
    let counters = Counters::new();
    c.bench_function("bench_counters_one_per_group", |b| {
        b.iter(|| {
            // Read one representative counter per group — 6 atomics
            // total. NOT a full snapshot; see
            // `bench_counters_full_snapshot` for that.
            let eth = counters.eth.rx_pkts.load(Ordering::Relaxed);
            let ip = counters.ip.rx_tcp.load(Ordering::Relaxed);
            let tcp = counters.tcp.tx_data.load(Ordering::Relaxed);
            let poll = counters.poll.iters.load(Ordering::Relaxed);
            let obs = counters.obs.events_dropped.load(Ordering::Relaxed);
            let fi = counters.fault_injector.drops.load(Ordering::Relaxed);
            black_box(
                eth.wrapping_add(ip)
                    .wrapping_add(tcp)
                    .wrapping_add(poll)
                    .wrapping_add(obs)
                    .wrapping_add(fi),
            );
        });
    });
}

fn bench_counters_full_snapshot(c: &mut Criterion) {
    let counters = Counters::new();
    c.bench_function("bench_counters_full_snapshot", |b| {
        b.iter(|| {
            // Read EVERY public atomic field on `Counters`. XOR-fold
            // into a single u64 accumulator and `black_box` once at
            // end so each load actually fires (no DCE).
            //
            // Field list mirrors `crates/dpdk-net-core/src/counters.rs`
            // struct declarations. Order tracks declaration order
            // within each group; refresh both sites together when
            // fields are added/removed.
            let mut acc: u64 = 0;

            // --- eth (38 AtomicU64) ---
            acc ^= counters.eth.rx_pkts.load(Ordering::Relaxed);
            acc ^= counters.eth.rx_bytes.load(Ordering::Relaxed);
            acc ^= counters.eth.rx_drop_miss_mac.load(Ordering::Relaxed);
            acc ^= counters.eth.rx_drop_nomem.load(Ordering::Relaxed);
            acc ^= counters.eth.tx_pkts.load(Ordering::Relaxed);
            acc ^= counters.eth.tx_bytes.load(Ordering::Relaxed);
            acc ^= counters.eth.tx_drop_full_ring.load(Ordering::Relaxed);
            acc ^= counters.eth.tx_drop_nomem.load(Ordering::Relaxed);
            acc ^= counters.eth.rx_drop_short.load(Ordering::Relaxed);
            acc ^= counters.eth.rx_drop_unknown_ethertype.load(Ordering::Relaxed);
            acc ^= counters.eth.rx_arp.load(Ordering::Relaxed);
            acc ^= counters.eth.tx_arp.load(Ordering::Relaxed);
            acc ^= counters.eth.offload_missing_rx_cksum_ipv4.load(Ordering::Relaxed);
            acc ^= counters.eth.offload_missing_rx_cksum_tcp.load(Ordering::Relaxed);
            acc ^= counters.eth.offload_missing_rx_cksum_udp.load(Ordering::Relaxed);
            acc ^= counters.eth.offload_missing_tx_cksum_ipv4.load(Ordering::Relaxed);
            acc ^= counters.eth.offload_missing_tx_cksum_tcp.load(Ordering::Relaxed);
            acc ^= counters.eth.offload_missing_tx_cksum_udp.load(Ordering::Relaxed);
            acc ^= counters.eth.offload_missing_mbuf_fast_free.load(Ordering::Relaxed);
            acc ^= counters.eth.offload_missing_rss_hash.load(Ordering::Relaxed);
            acc ^= counters.eth.offload_missing_llq.load(Ordering::Relaxed);
            acc ^= counters.eth.offload_missing_rx_timestamp.load(Ordering::Relaxed);
            acc ^= counters.eth.rx_drop_cksum_bad.load(Ordering::Relaxed);
            acc ^= counters.eth.llq_wc_missing.load(Ordering::Relaxed);
            acc ^= counters.eth.llq_header_overflow_risk.load(Ordering::Relaxed);
            acc ^= counters.eth.eni_bw_in_allowance_exceeded.load(Ordering::Relaxed);
            acc ^= counters.eth.eni_bw_out_allowance_exceeded.load(Ordering::Relaxed);
            acc ^= counters.eth.eni_pps_allowance_exceeded.load(Ordering::Relaxed);
            acc ^= counters.eth.eni_conntrack_allowance_exceeded.load(Ordering::Relaxed);
            acc ^= counters.eth.eni_linklocal_allowance_exceeded.load(Ordering::Relaxed);
            acc ^= counters.eth.tx_q0_linearize.load(Ordering::Relaxed);
            acc ^= counters.eth.tx_q0_doorbells.load(Ordering::Relaxed);
            acc ^= counters.eth.tx_q0_missed_tx.load(Ordering::Relaxed);
            acc ^= counters.eth.tx_q0_bad_req_id.load(Ordering::Relaxed);
            acc ^= counters.eth.rx_q0_refill_partial.load(Ordering::Relaxed);
            acc ^= counters.eth.rx_q0_bad_desc_num.load(Ordering::Relaxed);
            acc ^= counters.eth.rx_q0_bad_req_id.load(Ordering::Relaxed);
            acc ^= counters.eth.rx_q0_mbuf_alloc_fail.load(Ordering::Relaxed);

            // --- ip (12 AtomicU64) ---
            acc ^= counters.ip.rx_csum_bad.load(Ordering::Relaxed);
            acc ^= counters.ip.rx_ttl_zero.load(Ordering::Relaxed);
            acc ^= counters.ip.rx_frag.load(Ordering::Relaxed);
            acc ^= counters.ip.rx_icmp_frag_needed.load(Ordering::Relaxed);
            acc ^= counters.ip.pmtud_updates.load(Ordering::Relaxed);
            acc ^= counters.ip.rx_drop_short.load(Ordering::Relaxed);
            acc ^= counters.ip.rx_drop_bad_version.load(Ordering::Relaxed);
            acc ^= counters.ip.rx_drop_bad_hl.load(Ordering::Relaxed);
            acc ^= counters.ip.rx_drop_not_ours.load(Ordering::Relaxed);
            acc ^= counters.ip.rx_drop_unsupported_proto.load(Ordering::Relaxed);
            acc ^= counters.ip.rx_tcp.load(Ordering::Relaxed);
            acc ^= counters.ip.rx_icmp.load(Ordering::Relaxed);

            // --- tcp scalar AtomicU64 (61 fields) ---
            acc ^= counters.tcp.rx_syn_ack.load(Ordering::Relaxed);
            acc ^= counters.tcp.rx_data.load(Ordering::Relaxed);
            acc ^= counters.tcp.rx_ack.load(Ordering::Relaxed);
            acc ^= counters.tcp.rx_rst.load(Ordering::Relaxed);
            acc ^= counters.tcp.tx_retrans.load(Ordering::Relaxed);
            acc ^= counters.tcp.tx_rto.load(Ordering::Relaxed);
            acc ^= counters.tcp.tx_tlp.load(Ordering::Relaxed);
            acc ^= counters.tcp.tx_retrans_rto.load(Ordering::Relaxed);
            acc ^= counters.tcp.tx_retrans_rack.load(Ordering::Relaxed);
            acc ^= counters.tcp.tx_retrans_tlp.load(Ordering::Relaxed);
            acc ^= counters.tcp.conn_open.load(Ordering::Relaxed);
            acc ^= counters.tcp.conn_close.load(Ordering::Relaxed);
            acc ^= counters.tcp.conn_rst.load(Ordering::Relaxed);
            acc ^= counters.tcp.send_buf_full.load(Ordering::Relaxed);
            acc ^= counters.tcp.recv_buf_delivered.load(Ordering::Relaxed);
            acc ^= counters.tcp.tx_syn.load(Ordering::Relaxed);
            acc ^= counters.tcp.tx_ack.load(Ordering::Relaxed);
            acc ^= counters.tcp.tx_data.load(Ordering::Relaxed);
            acc ^= counters.tcp.tx_fin.load(Ordering::Relaxed);
            acc ^= counters.tcp.tx_rst.load(Ordering::Relaxed);
            acc ^= counters.tcp.rx_fin.load(Ordering::Relaxed);
            acc ^= counters.tcp.rx_unmatched.load(Ordering::Relaxed);
            acc ^= counters.tcp.rx_bad_csum.load(Ordering::Relaxed);
            acc ^= counters.tcp.rx_bad_flags.load(Ordering::Relaxed);
            acc ^= counters.tcp.rx_short.load(Ordering::Relaxed);
            acc ^= counters.tcp.recv_buf_drops.load(Ordering::Relaxed);
            acc ^= counters.tcp.rx_paws_rejected.load(Ordering::Relaxed);
            acc ^= counters.tcp.rx_bad_option.load(Ordering::Relaxed);
            acc ^= counters.tcp.rx_reassembly_queued.load(Ordering::Relaxed);
            acc ^= counters.tcp.rx_reassembly_hole_filled.load(Ordering::Relaxed);
            acc ^= counters.tcp.tx_sack_blocks.load(Ordering::Relaxed);
            acc ^= counters.tcp.rx_sack_blocks.load(Ordering::Relaxed);
            acc ^= counters.tcp.rx_bad_seq.load(Ordering::Relaxed);
            acc ^= counters.tcp.rx_bad_ack.load(Ordering::Relaxed);
            acc ^= counters.tcp.rx_dup_ack.load(Ordering::Relaxed);
            acc ^= counters.tcp.rx_zero_window.load(Ordering::Relaxed);
            acc ^= counters.tcp.rx_urgent_dropped.load(Ordering::Relaxed);
            acc ^= counters.tcp.tx_zero_window.load(Ordering::Relaxed);
            acc ^= counters.tcp.tx_window_update.load(Ordering::Relaxed);
            acc ^= counters.tcp.conn_table_full.load(Ordering::Relaxed);
            acc ^= counters.tcp.conn_time_wait_reaped.load(Ordering::Relaxed);
            acc ^= counters.tcp.tx_payload_bytes.load(Ordering::Relaxed);
            acc ^= counters.tcp.rx_payload_bytes.load(Ordering::Relaxed);
            acc ^= counters.tcp.conn_timeout_retrans.load(Ordering::Relaxed);
            acc ^= counters.tcp.conn_timeout_syn_sent.load(Ordering::Relaxed);
            acc ^= counters.tcp.rtt_samples.load(Ordering::Relaxed);
            acc ^= counters.tcp.tx_rack_loss.load(Ordering::Relaxed);
            acc ^= counters.tcp.rack_reo_wnd_override_active.load(Ordering::Relaxed);
            acc ^= counters.tcp.rto_no_backoff_active.load(Ordering::Relaxed);
            acc ^= counters.tcp.rx_ws_shift_clamped.load(Ordering::Relaxed);
            acc ^= counters.tcp.rx_dsack.load(Ordering::Relaxed);
            acc ^= counters.tcp.tx_tlp_spurious.load(Ordering::Relaxed);
            acc ^= counters.tcp.tx_api_timers_fired.load(Ordering::Relaxed);
            acc ^= counters.tcp.ts_recent_expired.load(Ordering::Relaxed);
            acc ^= counters.tcp.tx_flush_bursts.load(Ordering::Relaxed);
            acc ^= counters.tcp.tx_flush_batched_pkts.load(Ordering::Relaxed);
            acc ^= counters.tcp.rx_iovec_segs_total.load(Ordering::Relaxed);
            acc ^= counters.tcp.rx_multi_seg_events.load(Ordering::Relaxed);
            acc ^= counters.tcp.rx_partial_read_splits.load(Ordering::Relaxed);
            acc ^= counters.tcp.tx_persist.load(Ordering::Relaxed);
            acc ^= counters.tcp.mbuf_refcnt_drop_unexpected.load(Ordering::Relaxed);

            // --- tcp state_trans[11][11] matrix (121 AtomicU64) ---
            for row in &counters.tcp.state_trans {
                for cell in row {
                    acc ^= cell.load(Ordering::Relaxed);
                }
            }

            // --- tcp AtomicU32 diagnostics (2 fields) ---
            acc ^= counters.tcp.rx_mempool_avail.load(Ordering::Relaxed) as u64;
            acc ^= counters.tcp.tx_data_mempool_avail.load(Ordering::Relaxed) as u64;

            // --- poll (5 AtomicU64) ---
            acc ^= counters.poll.iters.load(Ordering::Relaxed);
            acc ^= counters.poll.iters_with_rx.load(Ordering::Relaxed);
            acc ^= counters.poll.iters_with_tx.load(Ordering::Relaxed);
            acc ^= counters.poll.iters_idle.load(Ordering::Relaxed);
            acc ^= counters.poll.iters_with_rx_burst_max.load(Ordering::Relaxed);

            // --- obs (2 AtomicU64) ---
            acc ^= counters.obs.events_dropped.load(Ordering::Relaxed);
            acc ^= counters.obs.events_queue_high_water.load(Ordering::Relaxed);

            // --- fault_injector (4 AtomicU64) ---
            acc ^= counters.fault_injector.drops.load(Ordering::Relaxed);
            acc ^= counters.fault_injector.dups.load(Ordering::Relaxed);
            acc ^= counters.fault_injector.reorders.load(Ordering::Relaxed);
            acc ^= counters.fault_injector.corrupts.load(Ordering::Relaxed);

            // Total: 38 + 12 + 61 + 121 + 2 + 5 + 2 + 4 = 245 atomic loads.
            black_box(acc);
        });
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(5))
        .sample_size(100);
    targets = bench_counters_one_per_group, bench_counters_full_snapshot
}
criterion_main!(benches);
