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
pub mod tcp_send_ack_log;
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
pub unsafe fn mbuf_data_slice<'a>(m: *mut dpdk_net_sys::rte_mbuf) -> &'a [u8] {
    let ptr = unsafe { dpdk_net_sys::shim_rte_pktmbuf_data(m) } as *const u8;
    let len = unsafe { dpdk_net_sys::shim_rte_pktmbuf_data_len(m) } as usize;
    unsafe { std::slice::from_raw_parts(ptr, len) }
}

/// PO12: prefetch the data area of `m` into L1 (`_MM_HINT_T0`). Used by
/// the RX burst dispatch loop to hide the L2/L3 miss latency of the
/// just-DMA'd mbuf payload — the NIC writes the payload bytes to host
/// memory cold of every CPU cache, so the first touch in the decode path
/// pays a full memory-side miss otherwise. Mirrors fstack's
/// `lib/ff_dpdk_if.c:2392-2408` `PREFETCH_OFFSET=3` pattern.
///
/// Calls `shim_rte_pktmbuf_data` to materialise the data-area pointer
/// (one cheap FFI hop into a single load + add inside the shim), then
/// emits a non-locking, non-fault-checking prefetch hint. A null
/// `m` yields a null data pointer; the prefetch instruction is a
/// no-op for unmapped / invalid addresses on x86_64, and the
/// fallback no-op path on non-x86_64 simply discards the pointer.
///
/// # Safety
///
/// `m` must be a valid mbuf pointer OR null (null is benign — the FFI
/// returns null which the prefetch instruction discards). The caller
/// must not rely on prefetch ordering — it is a CPU hint only.
/// The function passes `m` to `shim_rte_pktmbuf_data` which dereferences
/// the mbuf header — marking the function `unsafe` keeps that contract
/// visible at every callsite (per clippy::not_unsafe_ptr_arg_deref).
#[inline]
pub unsafe fn prefetch_mbuf_data(m: *mut dpdk_net_sys::rte_mbuf) {
    if m.is_null() {
        return;
    }
    // Safety: caller guarantees `m` is a valid mbuf pointer (or null,
    // checked above). `shim_rte_pktmbuf_data` reads the mbuf header
    // fields to compute the data-area pointer; the returned pointer
    // is a hint target only — no read or write through it occurs.
    let ptr = unsafe { dpdk_net_sys::shim_rte_pktmbuf_data(m) };
    #[cfg(target_arch = "x86_64")]
    unsafe {
        core::arch::x86_64::_mm_prefetch(
            ptr as *const i8,
            core::arch::x86_64::_MM_HINT_T0,
        );
    }
    #[cfg(target_arch = "aarch64")]
    unsafe {
        // `prfm pldl1keep` mirrors x86's `_MM_HINT_T0`: prefetch into
        // L1 for keep (read-stream hint). ARM intrinsics are
        // stable-since-1.59. Project has ARM on the roadmap (see
        // `feedback/project_arm_roadmap.md`); keeping the prefetch
        // active on ARM avoids a perf regression after the port.
        core::arch::aarch64::_prefetch(
            ptr as *const i8,
            core::arch::aarch64::_PREFETCH_READ,
            core::arch::aarch64::_PREFETCH_LOCALITY3,
        );
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        // No-op fallback on other archs: explicitly discard the
        // pointer so `unused_variables` doesn't lint.
        let _ = ptr;
    }
}
