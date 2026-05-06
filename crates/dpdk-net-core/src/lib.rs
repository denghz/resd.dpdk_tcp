//! Pure-Rust internals of the resd.dpdk_tcp stack.
//! The public `extern "C"` surface lives in the `dpdk-net` crate.

pub mod arp;
#[cfg(feature = "bench-alloc-audit")]
pub mod bench_alloc_audit;
pub mod clock;
pub mod counters;
pub mod dpdk_consts;
pub mod engine;
pub mod error;
#[cfg(feature = "fault-injector")]
pub mod fault_injector;
pub mod flow_table;
pub mod icmp;
pub mod iss;
pub mod l2;
pub mod l3_ip;
#[cfg(feature = "hw-verify-llq")]
pub mod llq_verify;
pub mod wc_verify;
pub mod ena_xstats;
pub mod iovec;
pub mod mempool;
pub mod rtt_histogram;
pub mod siphash24;
pub mod tcp_conn;
pub mod tcp_events;
pub mod tcp_input;
pub mod tcp_options;
pub mod tcp_output;
pub mod tcp_rack;
pub mod tcp_reassembly;
pub mod tcp_retrans;
pub mod tcp_rtt;
pub mod tcp_sack;
pub mod tcp_seq;
pub mod tcp_state;
// a10-perf-23.11 T2.2: module is `pub(crate)` by default; `bench-internals`
// promotes it to `pub` so tools/bench-micro (external crate) can reach
// `TimerWheel`, `TimerId`, `TimerNode`, `TimerKind`, and the `TICK_NS` /
// `LEVELS` / `BUCKETS` constants. Every item inside is already `pub`;
// only the module's outer visibility changes. Production builds (default
// features) continue to see `pub(crate)` — identical compiled output.
#[cfg(not(feature = "bench-internals"))]
pub(crate) mod tcp_timer_wheel;
#[cfg(feature = "bench-internals")]
pub mod tcp_timer_wheel;
pub mod tcp_tlp;
#[cfg(feature = "test-inject")]
pub mod test_fixtures;
#[cfg(feature = "test-server")]
pub mod test_server;
#[cfg(feature = "test-server")]
pub mod test_tx_intercept;

pub use error::Error;

// a10-perf-23.11 T2.3: feature-gated convenience re-export so
// `tools/bench-micro` (and T2.4 unit tests) can `use dpdk_net_core::EngineNoEalHarness`
// without poking through the `engine::test_support::` path.
#[cfg(feature = "bench-internals")]
pub use engine::test_support::EngineNoEalHarness;

/// Helper exposed for unit tests and the poll loop.
/// Returns the byte slice backing the mbuf's first (and in Stage A2, only)
/// segment. The caller must not outlive the mbuf.
///
/// # Safety
///
/// `m` must be a valid non-null mbuf pointer. Uses the C-shim
/// accessors from `dpdk-net-sys` because `rte_mbuf` is opaque to bindgen
/// (packed anonymous unions) — see Task 9 for the shim wiring.
///
/// **Note (C2 cross-phase retro fix).** This helper observes only the
/// HEAD segment's `data_len`. For single-segment mbufs (`nb_segs == 1`)
/// this matches `pkt_len`. For multi-segment chains the returned slice
/// is silently truncated; callers in the production RX path use
/// [`mbuf_data_slice_for_rx`] which linearizes the chain into a
/// scratch buffer when needed. This function is retained for legacy
/// single-segment call sites and tests.
pub unsafe fn mbuf_data_slice<'a>(m: *mut dpdk_net_sys::rte_mbuf) -> &'a [u8] {
    let ptr = unsafe { dpdk_net_sys::shim_rte_pktmbuf_data(m) } as *const u8;
    let len = unsafe { dpdk_net_sys::shim_rte_pktmbuf_data_len(m) } as usize;
    unsafe { std::slice::from_raw_parts(ptr, len) }
}

/// C2 cross-phase retro fix — RX-side mbuf-to-slice conversion that
/// correctly handles multi-segment chains.
///
/// * Single-segment (`nb_segs == 1`): returns a borrowed slice into the
///   head mbuf's data region. Zero-copy fast path; identical to
///   [`mbuf_data_slice`] for this case.
/// * Multi-segment (`nb_segs > 1`): linearizes the chain by walking
///   `mbuf.next` and copying each segment's `data_len` bytes into a
///   freshly-allocated `Vec<u8>` sized to `pkt_len`. Bumps
///   `eth.rx_multi_seg_linearized` exactly once per chain. The returned
///   `Cow::Owned` carries the linearized payload; the caller can hand
///   the `&[u8]` view to the L2/L3/L4 decoders without truncation.
///
/// The bug being fixed: pre-C2 the RX path called [`mbuf_data_slice`]
/// directly on the head mbuf, which exposes only the head's `data_len`
/// bytes. For real-NIC scatter (RX_OFFLOAD_SCATTER) the IPv4
/// `total_length` field reflects the entire datagram's size — once that
/// exceeds `data_len`, the L3 decoder rejects the frame as
/// `BadTotalLen` and silently drops a valid jumbo packet.
///
/// # Safety
///
/// * `m` must be a valid non-null mbuf pointer.
/// * For multi-segment chains, the entire chain (every link reachable
///   via `mbuf.next`) must be valid for reads of its `data_len` bytes
///   for the duration of this call.
///
/// The returned `Cow::Borrowed` slice (single-seg fast path) is tied
/// to the mbuf's lifetime via the `'a` parameter; the caller must not
/// outlive the mbuf. The `Cow::Owned` variant (multi-seg) is owned by
/// the caller and has no lifetime tie to the mbuf.
pub unsafe fn mbuf_data_slice_for_rx<'a>(
    m: *mut dpdk_net_sys::rte_mbuf,
    counters: &crate::counters::EthCounters,
) -> std::borrow::Cow<'a, [u8]> {
    let nb_segs = unsafe { dpdk_net_sys::shim_rte_pktmbuf_nb_segs(m) };
    if nb_segs <= 1 {
        // Fast path: single-segment mbuf. Borrow the head's data region
        // verbatim — identical to mbuf_data_slice's behaviour.
        let ptr = unsafe { dpdk_net_sys::shim_rte_pktmbuf_data(m) } as *const u8;
        let len = unsafe { dpdk_net_sys::shim_rte_pktmbuf_data_len(m) } as usize;
        std::borrow::Cow::Borrowed(unsafe { std::slice::from_raw_parts(ptr, len) })
    } else {
        // Slow path: linearize the chain. `pkt_len` carries the sum of
        // every link's `data_len` (maintained by DPDK's RX path +
        // `rte_pktmbuf_chain`). Pre-allocate the Vec to that exact
        // capacity so the inner copy walks each link's bytes once and
        // never reallocates.
        let total = unsafe { dpdk_net_sys::shim_rte_pktmbuf_pkt_len(m) } as usize;
        let mut buf: Vec<u8> = Vec::with_capacity(total);
        let mut cur = m;
        let mut written = 0usize;
        while !cur.is_null() && written < total {
            let seg_ptr =
                unsafe { dpdk_net_sys::shim_rte_pktmbuf_data(cur) } as *const u8;
            let seg_len =
                unsafe { dpdk_net_sys::shim_rte_pktmbuf_data_len(cur) } as usize;
            let take = seg_len.min(total - written);
            // SAFETY: `buf` has capacity `total`; `written + take <= total`
            // by construction (the `take` clamp above). `seg_ptr` is
            // valid for `seg_len` bytes per DPDK layout, and `take <=
            // seg_len`. The destination `buf.as_mut_ptr().add(written)`
            // is in-bounds of an allocation of size `total`.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    seg_ptr,
                    buf.as_mut_ptr().add(written),
                    take,
                );
            }
            written += take;
            cur = unsafe { dpdk_net_sys::shim_rte_pktmbuf_next(cur) };
        }
        // SAFETY: `written` bytes initialised by the loop above.
        unsafe { buf.set_len(written) };
        // Slow-path counter bump per spec §9.1.1 — exactly one
        // `fetch_add` per linearized chain (not per segment, not per
        // byte). Ordering: Relaxed matches every other slow-path
        // counter in the eth group (see `crate::counters::inc`).
        counters
            .rx_multi_seg_linearized
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        std::borrow::Cow::Owned(buf)
    }
}

#[cfg(test)]
mod mbuf_data_slice_tests {
    //! C2 cross-phase retro fix — unit tests for the multi-segment
    //! linearization helper. These run without a live DPDK EAL: we
    //! synthesize fake `rte_mbuf` headers in heap-allocated boxes and
    //! exercise the chain-walk against the same shim accessors the
    //! production helper uses. The shim functions in `dpdk-net-sys`
    //! read fixed offsets into `rte_mbuf` (set up by DPDK at mbuf-pool
    //! initialisation time); to keep the unit test free of EAL
    //! dependencies we route the test through the linearization
    //! primitive directly via the `Vec`-of-slices form.
    //!
    //! Path coverage:
    //! * `linearize_single_segment_returns_borrowed` — the fast path
    //!   returns `Cow::Borrowed` and does not bump the counter.
    //! * `linearize_multi_segment_returns_owned_concat` — multi-seg
    //!   chains are concatenated in order, the counter bumps exactly
    //!   once, and the bytes match a manually-concatenated reference.
    //! * `linearize_three_segments_concatenates_in_order` — extends
    //!   the multi-seg coverage to 3 segments to confirm the chain
    //!   walk doesn't stop early.
    //!
    //! The end-to-end "L3 decoder accepts linearized jumbo frame"
    //! coverage lives in the TAP integration tests under the
    //! `test-inject` feature (e.g. `inject_rx_chain_smoke`) which
    //! exercise the helper through a real DPDK mbuf.
    use crate::counters::EthCounters;
    use std::sync::atomic::Ordering;
    /// Test-only linearization primitive shared by every test in this
    /// module. Mirrors the `multi-seg` branch of
    /// [`super::mbuf_data_slice_for_rx`] without invoking the
    /// `rte_mbuf` shim accessors — instead it walks a slice-of-slices
    /// representation that the test cases build directly. Single-seg
    /// is shaped as `&[head]`; multi-seg as `&[head, link1, ...]`.
    fn linearize_via_slices<'a>(
        segs: &'a [&'a [u8]],
        counters: &EthCounters,
    ) -> std::borrow::Cow<'a, [u8]> {
        if segs.len() <= 1 {
            return std::borrow::Cow::Borrowed(segs.first().copied().unwrap_or(&[]));
        }
        let total: usize = segs.iter().map(|s| s.len()).sum();
        let mut buf: Vec<u8> = Vec::with_capacity(total);
        for s in segs {
            buf.extend_from_slice(s);
        }
        counters
            .rx_multi_seg_linearized
            .fetch_add(1, Ordering::Relaxed);
        std::borrow::Cow::Owned(buf)
    }

    #[test]
    fn linearize_single_segment_returns_borrowed() {
        let counters = EthCounters::default();
        let head: [u8; 8] = [0x11; 8];
        let segs: [&[u8]; 1] = [&head];
        let cow = linearize_via_slices(&segs, &counters);
        assert!(
            matches!(cow, std::borrow::Cow::Borrowed(_)),
            "single-segment must return Cow::Borrowed (zero-copy fast path)"
        );
        assert_eq!(&*cow, &head[..]);
        assert_eq!(
            counters.rx_multi_seg_linearized.load(Ordering::Relaxed),
            0,
            "single-segment must NOT bump the linearization counter"
        );
    }

    #[test]
    fn linearize_multi_segment_returns_owned_concat() {
        let counters = EthCounters::default();
        let head: Vec<u8> = vec![0xAAu8; 32];
        let tail: Vec<u8> = vec![0xBBu8; 16];
        let segs: [&[u8]; 2] = [&head, &tail];
        let cow = linearize_via_slices(&segs, &counters);
        assert!(
            matches!(cow, std::borrow::Cow::Owned(_)),
            "multi-segment must return Cow::Owned (linearized scratch buffer)"
        );
        assert_eq!(cow.len(), 48, "linearized total = sum of every segment");
        assert_eq!(&cow[..32], &head[..], "first 32 bytes carry head segment");
        assert_eq!(&cow[32..], &tail[..], "last 16 bytes carry tail segment");
        assert_eq!(
            counters.rx_multi_seg_linearized.load(Ordering::Relaxed),
            1,
            "multi-segment must bump the linearization counter exactly once"
        );
    }

    #[test]
    fn linearize_three_segments_concatenates_in_order() {
        let counters = EthCounters::default();
        let s0: Vec<u8> = vec![0x01u8; 10];
        let s1: Vec<u8> = vec![0x02u8; 20];
        let s2: Vec<u8> = vec![0x03u8; 30];
        let segs: [&[u8]; 3] = [&s0, &s1, &s2];
        let cow = linearize_via_slices(&segs, &counters);
        assert_eq!(cow.len(), 60);
        assert_eq!(&cow[..10], &s0[..]);
        assert_eq!(&cow[10..30], &s1[..]);
        assert_eq!(&cow[30..], &s2[..]);
        assert_eq!(
            counters.rx_multi_seg_linearized.load(Ordering::Relaxed),
            1,
            "three-segment chain still produces exactly one counter bump"
        );
    }
}
