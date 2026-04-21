//! Integration tests for mode A (RTT comparison) pure-Rust primitives.
//!
//! These tests don't touch DPDK or a live peer — they validate the
//! library façade (`Stack`, `Mode`, dimensions JSON shape, AF_PACKET
//! stub surface) so they run on any host without an ENA VF, EAL
//! init, or peer echo-server.
//!
//! The real end-to-end smoke (dpdk + linux + peer) lives post-AMI
//! bake (Plan A T6+T7); see the parent plan for the sister IaC
//! project that provisions the sister peer instance.

use bench_vs_linux::{
    afpacket::{self, AfPacketConfig, AfPacketError},
    mode_rtt, mode_wire_diff, Mode, Stack,
};

// ---------------------------------------------------------------------------
// Mode selector + stack parser.
// ---------------------------------------------------------------------------

#[test]
fn mode_rtt_and_wire_diff_both_parse() {
    assert_eq!(Mode::parse("rtt").unwrap(), Mode::Rtt);
    assert_eq!(Mode::parse("wire-diff").unwrap(), Mode::WireDiff);
    assert_eq!(Mode::parse("wire_diff").unwrap(), Mode::WireDiff);
    assert!(Mode::parse("garbage").is_err());
}

#[test]
fn stack_parse_covers_all_three_canonical_tokens() {
    assert_eq!(Stack::parse("dpdk").unwrap(), Stack::DpdkNet);
    assert_eq!(Stack::parse("linux").unwrap(), Stack::LinuxKernel);
    assert_eq!(Stack::parse("afpacket").unwrap(), Stack::AfPacket);
}

#[test]
fn stack_as_dimension_is_the_documented_string() {
    // Bench-report groups by the verbatim dimensions_json.stack value,
    // so any rename here is a breaking schema change.
    assert_eq!(Stack::DpdkNet.as_dimension(), "dpdk_net");
    assert_eq!(Stack::LinuxKernel.as_dimension(), "linux_kernel");
    assert_eq!(Stack::AfPacket.as_dimension(), "afpacket");
}

// ---------------------------------------------------------------------------
// AF_PACKET stub — validate the frame-shape primitives + stub error.
// ---------------------------------------------------------------------------

#[test]
fn afpacket_stub_run_rtt_workload_returns_unimplemented() {
    let cfg = AfPacketConfig {
        iface: "ens6",
        peer_ip_host_order: 0x0A00_002A,
        peer_port: 10_001,
        request_bytes: 128,
        response_bytes: 128,
        warmup: 10,
        iterations: 100,
    };
    match afpacket::run_rtt_workload(&cfg) {
        Err(AfPacketError::Unimplemented) => {}
        other => panic!("expected AfPacketError::Unimplemented, got {other:?}"),
    }
}

#[test]
fn afpacket_validate_config_rejects_malformed_inputs() {
    // Each field gets an isolated failure test in the module-side unit
    // tests; here we sanity-check the public validator is exposed.
    let ok = AfPacketConfig {
        iface: "ens6",
        peer_ip_host_order: 0x0A00_002A,
        peer_port: 10_001,
        request_bytes: 128,
        response_bytes: 128,
        warmup: 10,
        iterations: 100,
    };
    assert!(afpacket::validate_config(&ok).is_ok());

    let bad = AfPacketConfig { iface: "", ..ok };
    assert!(afpacket::validate_config(&bad).is_err());
}

#[test]
fn afpacket_min_frame_len_matches_ethernet_plus_min_ip_tcp() {
    // Ethernet II (14) + IPv4 min (20) + TCP min (20) = 54.
    assert_eq!(afpacket::min_frame_len(), 54);
}

// ---------------------------------------------------------------------------
// Mode B stub — wire-diff.
// ---------------------------------------------------------------------------

#[test]
fn mode_wire_diff_stub_errors_with_t9_pointer() {
    let err = mode_wire_diff::run_mode_wire_diff()
        .unwrap_err()
        .to_string();
    assert!(
        err.contains("T9") || err.contains("Task 9"),
        "mode B stub must point at T9: {err}"
    );
}

// ---------------------------------------------------------------------------
// mode_rtt dimensions_json shape.
// ---------------------------------------------------------------------------

#[test]
fn mode_rtt_dimensions_json_tags_preset_mode_stack() {
    for stack in [Stack::DpdkNet, Stack::LinuxKernel, Stack::AfPacket] {
        let dims = mode_rtt::build_dimensions_json(stack);
        let parsed: serde_json::Value = serde_json::from_str(&dims).unwrap();
        assert_eq!(parsed["preset"], "latency");
        assert_eq!(parsed["mode"], "rtt");
        assert_eq!(parsed["stack"], stack.as_dimension());
    }
}

#[test]
fn mode_rtt_dimensions_json_is_deterministic() {
    // Deterministic output is required — bench-report groups by the
    // verbatim string, so a reordered JSON would fragment aggregates.
    let a = mode_rtt::build_dimensions_json(Stack::DpdkNet);
    let b = mode_rtt::build_dimensions_json(Stack::DpdkNet);
    assert_eq!(a, b);
}
