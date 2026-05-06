//! End-to-end FFI smoke test: uses the public C ABI from Rust to prove the
//! extern "C" surface is usable, not just the Rust-native one.
//!
//! - `ffi_handles_null_safely` runs always.
//! - `ffi_eal_init_and_engine_lifecycle` runs only when DPDK_NET_TEST_TAP=1
//!   because it actually initializes EAL against a DPDK TAP vdev.

use std::ffi::CString;
use std::ptr;

#[link(name = "dpdk_net", kind = "static")]
extern "C" {
    fn dpdk_net_eal_init(argc: i32, argv: *const *const libc::c_char) -> i32;
    fn dpdk_net_engine_create(
        lcore_id: u16,
        cfg: *const core::ffi::c_void,
    ) -> *mut core::ffi::c_void;
    fn dpdk_net_engine_destroy(p: *mut core::ffi::c_void);
    fn dpdk_net_poll(
        p: *mut core::ffi::c_void,
        events_out: *mut core::ffi::c_void,
        max_events: u32,
        timeout_ns: u64,
    ) -> i32;
    fn dpdk_net_now_ns(p: *mut core::ffi::c_void) -> u64;
}

#[test]
fn ffi_handles_null_safely() {
    unsafe {
        dpdk_net_engine_destroy(ptr::null_mut());
        let rc = dpdk_net_poll(ptr::null_mut(), ptr::null_mut(), 0, 0);
        assert_eq!(rc, -libc::EINVAL);
        let ts = dpdk_net_now_ns(ptr::null_mut());
        assert!(ts > 0);
    }
}

#[test]
fn ffi_eal_init_and_engine_lifecycle() {
    if std::env::var("DPDK_NET_TEST_TAP").ok().as_deref() != Some("1") {
        eprintln!("skipping; set DPDK_NET_TEST_TAP=1 to run");
        return;
    }

    let args: Vec<CString> = [
        "dpdk-net-ffi-test",
        "--in-memory",
        "--no-pci",
        "--vdev=net_tap0,iface=dpdktap0",
        "-l",
        "0-1",
        "--log-level=3",
    ]
    .iter()
    .map(|s| CString::new(*s).unwrap())
    .collect();
    let argv: Vec<*const libc::c_char> = args.iter().map(|c| c.as_ptr()).collect();

    let rc = unsafe { dpdk_net_eal_init(argv.len() as i32, argv.as_ptr()) };
    assert_eq!(rc, 0, "dpdk_net_eal_init failed: {rc}");

    // A5 cross-phase fix: use the real Rust type instead of a hand-rolled
    // byte-shim. Manual mirroring drifted silently when `rx_mempool_size`
    // was appended in A6.6-7 Task 10 — `dpdk_net_engine_create` was reading
    // uninitialized memory past the end of the shorter shim. Zero-init the
    // whole struct, then override only the fields the test exercises.
    let mut cfg = unsafe {
        std::mem::zeroed::<dpdk_net::api::dpdk_net_engine_config_t>()
    };
    cfg.max_connections = 16;
    cfg.recv_buffer_bytes = 256 * 1024;
    cfg.send_buffer_bytes = 256 * 1024;
    cfg.tcp_timestamps = true;
    cfg.tcp_sack = true;
    cfg.tcp_min_rto_us = 5_000;
    cfg.tcp_initial_rto_us = 5_000;
    cfg.tcp_max_rto_us = 1_000_000;
    cfg.tcp_max_retrans_count = 15;
    cfg.tcp_msl_ms = 30000;
    cfg.event_queue_soft_cap = 4096;

    let eng = unsafe {
        dpdk_net_engine_create(
            0,
            &cfg as *const dpdk_net::api::dpdk_net_engine_config_t as *const core::ffi::c_void,
        )
    };
    assert!(!eng.is_null(), "dpdk_net_engine_create returned null");

    for _ in 0..10 {
        let rc = unsafe { dpdk_net_poll(eng, ptr::null_mut(), 0, 0) };
        assert_eq!(rc, 0);
    }

    unsafe { dpdk_net_engine_destroy(eng) };
}
