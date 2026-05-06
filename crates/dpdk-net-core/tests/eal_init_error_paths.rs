//! Tests for Task A2: eal_init returns errors instead of panicking.

use std::panic::catch_unwind;

#[test]
fn eal_init_with_nul_byte_in_arg_returns_err_not_panic() {
    let bad_arg = "prefix\0suffix";
    let result = catch_unwind(|| {
        dpdk_net_core::engine::eal_init(&["eal_test", bad_arg])
    });
    let inner = result.expect("eal_init panicked — unwrap was not removed");
    assert!(inner.is_err(), "expected Err on NUL-byte arg, got Ok");
}

#[test]
fn eal_init_with_nul_byte_in_arg_returns_argv_nul_error() {
    let result = dpdk_net_core::engine::eal_init(&["eal_test", "no\0null"]);
    assert!(matches!(result, Err(dpdk_net_core::Error::ArgvNul)));
}
