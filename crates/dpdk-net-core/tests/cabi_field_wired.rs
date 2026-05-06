//! TDD tests for Task A1: verify tcp_timestamps/tcp_sack config flags are wired
//! into build_connect_syn_opts so setting them to false actually suppresses the options.

// These tests call the (pub-for-test) helper directly so they run without EAL/DPDK.
// They are compiled only when the `test` profile is active.

use dpdk_net_core::engine::EngineConfig;

// We need a way to call build_connect_syn_opts with controlled config.
// The plan says: add `Engine::build_connect_syn_opts_for_test(&cfg)`.
// Implement a thin pub(crate) or #[cfg(test)] wrapper in engine.rs.

#[test]
fn tcp_timestamps_false_omits_ts_option() {
    let mut cfg = EngineConfig::default();
    cfg.tcp_timestamps = false;
    let opts = dpdk_net_core::engine::build_connect_syn_opts_for_test(
        cfg.recv_buffer_bytes,
        cfg.tcp_mss as u16,
        1_000_000,
        cfg.tcp_timestamps,
        cfg.tcp_sack,
    );
    assert!(opts.timestamps.is_none(), "TS option emitted despite tcp_timestamps=false");
}

#[test]
fn tcp_sack_false_omits_sack_permitted() {
    let mut cfg = EngineConfig::default();
    cfg.tcp_sack = false;
    let opts = dpdk_net_core::engine::build_connect_syn_opts_for_test(
        cfg.recv_buffer_bytes,
        cfg.tcp_mss as u16,
        1_000_000,
        cfg.tcp_timestamps,
        cfg.tcp_sack,
    );
    assert!(!opts.sack_permitted, "SACK-permitted emitted despite tcp_sack=false");
}

#[test]
fn tcp_timestamps_true_includes_ts_option() {
    let cfg = EngineConfig::default(); // default should have tcp_timestamps=true
    let opts = dpdk_net_core::engine::build_connect_syn_opts_for_test(
        cfg.recv_buffer_bytes,
        cfg.tcp_mss as u16,
        1_000_000,
        cfg.tcp_timestamps,
        cfg.tcp_sack,
    );
    assert!(opts.timestamps.is_some(), "TS option missing despite tcp_timestamps=true");
}

#[test]
fn tcp_sack_true_includes_sack_permitted() {
    let cfg = EngineConfig::default(); // default should have tcp_sack=true
    let opts = dpdk_net_core::engine::build_connect_syn_opts_for_test(
        cfg.recv_buffer_bytes,
        cfg.tcp_mss as u16,
        1_000_000,
        cfg.tcp_timestamps,
        cfg.tcp_sack,
    );
    assert!(opts.sack_permitted, "SACK-permitted missing despite tcp_sack=true");
}
