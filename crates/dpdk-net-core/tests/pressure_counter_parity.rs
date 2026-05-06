//! Pressure Suite 3 — `pressure-counter-parity-offload-matrix` (A11.1 Lane C).
//!
//! Drives injected frames through the engine's real decode path and asserts
//! that each checksum-drop counter is incremented EXACTLY ONCE PER DROP along
//! both the NIC-offload path and the software-verify path.  Addresses the
//! B1-class bug (ip.rx_csum_bad double-bump surfaced by Part 1 BLOCK-A11 #3)
//! and replaces synthetic `bump_counter_one_shot` coverage for the checksum
//! counter group.
//!
//! ## Four rows
//!
//! | Row | Feature gate | Path | Frame | ol_flags |
//! |-----|-------------|------|-------|---------|
//! | A | hw-offload-rx-cksum | NIC IP-BAD  | valid  | RTE_MBUF_F_RX_IP_CKSUM_BAD (0x10) |
//! | B | hw-offload-rx-cksum | NIC L4-BAD  | valid  | RTE_MBUF_F_RX_L4_CKSUM_BAD (0x08) |
//! | C | (always)            | SW IP-BAD   | bad IP cksum bytes | 0 |
//! | D | (always)            | SW TCP-BAD  | bad TCP cksum bytes | 0 |
//!
//! ## Expected counter deltas per N_INJECT frames
//!
//! | Row | eth.rx_drop_cksum_bad | ip.rx_csum_bad | tcp.rx_bad_csum |
//! |-----|----------------------|----------------|-----------------|
//! | A   | N                    | N              | 0               |
//! | B   | N                    | 0              | N               |
//! | C   | 0                    | N              | 0               |
//! | D   | 0                    | 0              | N               |
//!
//! Row A reflects the post-B1 single-bump: `l3_ip.rs` bumps
//! `eth.rx_drop_cksum_bad` and returns `L3Drop::CsumBad`; `engine.rs`
//! `handle_ipv4` bumps `ip.rx_csum_bad`. Before B1 there were two sites
//! bumping `ip.rx_csum_bad` — the test detects a regression if that
//! double-bump returns.
//!
//! ## TAP gate
//!
//! EAL initialisation requires the `DPDK_NET_TEST_TAP=1` environment
//! variable + sudo (TAP vdev creation).  Without it the test returns early
//! with an informational `eprintln!`.
//!
//! ## Feature gates
//!
//! * `pressure-test` — `PressureBucket` / `CounterSnapshot` DSL.
//! * `test-inject`   — `Engine::inject_rx_frame` and
//!   `Engine::inject_rx_frame_with_ol_flags`.
//! * `hw-offload-rx-cksum` (rows A, B only) — rows A/B compile away when
//!   this feature is absent so the test still passes in offload-OFF builds.

#![cfg(all(feature = "pressure-test", feature = "test-inject"))]

mod common;
use common::pressure::{assert_delta, PressureBucket, Relation};

use dpdk_net_core::engine::{eal_init, Engine, EngineConfig};

use std::panic::AssertUnwindSafe;
use std::process::Command;
use std::thread;
use std::time::Duration;

const TAP_IFACE: &str = "resdtap33";
const OUR_IP: u32 = 0x0a_63_21_02; // 10.99.33.2
const PEER_IP: u32 = 0x0a_63_21_01; // 10.99.33.1
const OUR_IP_STR: &str = "10.99.33.2";

const N_INJECT: u64 = 100;

// DPDK RX mbuf offload flag constants (from bindings.rs).
const RTE_MBUF_F_RX_IP_CKSUM_BAD: u64 = 16; // 0x10
const RTE_MBUF_F_RX_L4_CKSUM_BAD: u64 = 8;  // 0x08

fn skip_if_not_tap() -> bool {
    if std::env::var("DPDK_NET_TEST_TAP").ok().as_deref() != Some("1") {
        eprintln!("skipping; set DPDK_NET_TEST_TAP=1 to run (requires sudo for TAP vdev)");
        return true;
    }
    false
}

fn read_kernel_tap_mac(iface: &str) -> [u8; 6] {
    let path = format!("/sys/class/net/{iface}/address");
    let s = std::fs::read_to_string(&path).unwrap_or_else(|_| panic!("read {path}"));
    let mut out = [0u8; 6];
    for (i, part) in s.trim().split(':').enumerate() {
        out[i] = u8::from_str_radix(part, 16).expect("hex mac");
    }
    out
}

fn bring_up_tap(iface: &str) {
    let _ = Command::new("ip")
        .args(["link", "set", iface, "up"])
        .status();
    let _ = Command::new("ip")
        .args(["addr", "add", "10.99.33.1/24", "dev", iface])
        .status();
}

fn pin_arp(iface: &str, ip: &str, mac: &str) {
    let _ = Command::new("ip")
        .args([
            "neigh", "replace", ip, "lladdr", mac, "dev", iface, "nud", "permanent",
        ])
        .status();
}

fn mac_hex(mac: [u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

/// Internet checksum: one's-complement sum of all 16-bit words in `data`.
/// If `data` has an odd number of bytes the last byte is zero-padded.
fn internet_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        let word = u16::from_be_bytes([data[i], data[i + 1]]);
        sum += word as u32;
        i += 2;
    }
    if i < data.len() {
        sum += (data[i] as u32) << 8;
    }
    while sum > 0xFFFF {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// Build a minimal 54-byte Ethernet+IPv4+TCP frame.
///
/// `corrupt_ip_cksum` zeroes the IP checksum field (SW IP-BAD, Row C).
/// `corrupt_tcp_cksum` zeroes the TCP checksum field (SW TCP-BAD, Row D).
/// When both flags are false the frame has valid checksums in both headers
/// (for NIC-offload rows A/B where correctness is reported via `ol_flags`).
fn build_minimal_tcp_frame(
    dst_mac: [u8; 6],
    src_ip: u32,
    dst_ip: u32,
    src_port: u16,
    dst_port: u16,
    corrupt_ip_cksum: bool,
    corrupt_tcp_cksum: bool,
) -> Vec<u8> {
    let mut frame = Vec::with_capacity(54);

    // ─ Ethernet header (14 bytes) ────────────────────────────────────
    frame.extend_from_slice(&dst_mac);          // dst MAC = our engine MAC
    frame.extend_from_slice(&[0x52, 0x54, 0x00, 0x12, 0x34, 0x56]); // src MAC
    frame.extend_from_slice(&[0x08, 0x00]);     // ethertype = IPv4

    // ─ IPv4 header (20 bytes) ────────────────────────────────────────
    let tcp_total_len: u16 = 20 + 20; // TCP header only, no payload
    let ip_total_len: u16 = 20 + tcp_total_len;
    let ip_hdr_no_cksum: [u8; 20] = [
        0x45,                                   // version=4, IHL=5
        0x00,                                   // DSCP/ECN
        (ip_total_len >> 8) as u8,
        ip_total_len as u8,
        0x00, 0x00,                             // ID
        0x40, 0x00,                             // flags=DF, frag=0
        0x40,                                   // TTL=64
        0x06,                                   // proto=TCP
        0x00, 0x00,                             // checksum (will fill)
        ((src_ip >> 24) & 0xFF) as u8,
        ((src_ip >> 16) & 0xFF) as u8,
        ((src_ip >> 8)  & 0xFF) as u8,
        ( src_ip        & 0xFF) as u8,
        ((dst_ip >> 24) & 0xFF) as u8,
        ((dst_ip >> 16) & 0xFF) as u8,
        ((dst_ip >> 8)  & 0xFF) as u8,
        ( dst_ip        & 0xFF) as u8,
    ];
    let ip_cksum = internet_checksum(&ip_hdr_no_cksum);
    let mut ip_hdr = ip_hdr_no_cksum;
    if corrupt_ip_cksum {
        ip_hdr[10] = 0xFF; // invalid checksum
        ip_hdr[11] = 0xFF;
    } else {
        ip_hdr[10] = (ip_cksum >> 8) as u8;
        ip_hdr[11] = ip_cksum as u8;
    }
    frame.extend_from_slice(&ip_hdr);

    // ─ TCP header (20 bytes) ─────────────────────────────────────────
    // Pseudo-header for TCP checksum computation.
    let tcp_hdr_no_cksum: [u8; 20] = [
        (src_port >> 8) as u8, src_port as u8,
        (dst_port >> 8) as u8, dst_port as u8,
        0x00, 0x00, 0x00, 0x01, // seq=1
        0x00, 0x00, 0x00, 0x00, // ack=0
        0x50,                   // data offset = 5 (20 bytes), reserved=0
        0x10,                   // flags: ACK
        0x04, 0x00,             // window = 1024
        0x00, 0x00,             // checksum (will fill)
        0x00, 0x00,             // urgent pointer
    ];
    let pseudo: Vec<u8> = {
        let mut p = Vec::with_capacity(12 + 20);
        p.extend_from_slice(&ip_hdr_no_cksum[12..16]); // src_ip
        p.extend_from_slice(&ip_hdr_no_cksum[16..20]); // dst_ip
        p.push(0x00);                                   // zero
        p.push(0x06);                                   // proto = TCP
        p.push((tcp_total_len >> 8) as u8);
        p.push(tcp_total_len as u8);
        p.extend_from_slice(&tcp_hdr_no_cksum);
        p
    };
    let tcp_cksum = internet_checksum(&pseudo);
    let mut tcp_hdr = tcp_hdr_no_cksum;
    if corrupt_tcp_cksum {
        tcp_hdr[16] = 0xFF; // invalid checksum
        tcp_hdr[17] = 0xFF;
    } else {
        tcp_hdr[16] = (tcp_cksum >> 8) as u8;
        tcp_hdr[17] = tcp_cksum as u8;
    }
    frame.extend_from_slice(&tcp_hdr);

    frame
}

#[test]
fn pressure_counter_parity_4_rows() {
    if skip_if_not_tap() {
        return;
    }

    let args = [
        "dpdk-net-pressure-cksum-parity",
        "--in-memory",
        "--no-pci",
        "--vdev=net_tap0,iface=resdtap33",
        "-l",
        "0-1",
        "--log-level=3",
    ];
    eal_init(&args).expect("EAL init");

    bring_up_tap(TAP_IFACE);
    thread::sleep(Duration::from_millis(500));

    let kernel_mac = read_kernel_tap_mac(TAP_IFACE);

    let cfg = EngineConfig {
        port_id: 0,
        local_ip: OUR_IP,
        gateway_ip: PEER_IP,
        gateway_mac: kernel_mac,
        tcp_mss: 1460,
        max_connections: 4,
        tcp_msl_ms: 100,
        ..Default::default()
    };
    let engine = Engine::new(cfg.clone()).expect("engine new");
    let our_mac = engine.our_mac();
    pin_arp(TAP_IFACE, OUR_IP_STR, &mac_hex(our_mac));

    // Synthetic port for injected frames — does not need to match an active
    // connection; cksum drops occur before connection lookup.
    let src_port: u16 = 55555;
    let dst_port: u16 = 9999; // arbitrary, no connection on this port

    // ────────────────────────────────────────────────────────────────
    // Row C — software IP-cksum bad (works regardless of hw offload)
    // ────────────────────────────────────────────────────────────────
    {
        let frame = build_minimal_tcp_frame(
            our_mac, PEER_IP, OUR_IP, src_port, dst_port,
            /* corrupt_ip */ true, /* corrupt_tcp */ false,
        );
        let bucket = PressureBucket::open("pressure-counter-parity", "row_c_sw_ip_bad", engine.counters());
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            for _ in 0..N_INJECT {
                engine.inject_rx_frame(&frame).expect("inject row C");
            }
            let after = common::pressure::CounterSnapshot::capture(engine.counters());
            let delta = after.delta_since(&bucket.before);
            assert_delta(&delta, "eth.rx_drop_cksum_bad", Relation::Eq(0));
            assert_delta(&delta, "ip.rx_csum_bad", Relation::Eq(N_INJECT as i64));
            assert_delta(&delta, "tcp.rx_bad_csum", Relation::Eq(0));
            assert_delta(&delta, "tcp.mbuf_refcnt_drop_unexpected", Relation::Eq(0));
            assert_delta(&delta, "obs.events_dropped", Relation::Eq(0));
        }));
        match result {
            Ok(()) => bucket.finish_ok(),
            Err(e) => {
                let msg = e.downcast_ref::<String>().cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "<non-string panic>".to_string());
                let path = bucket.finish_fail(engine.counters(), &cfg, vec![], msg.clone());
                panic!("Row C failed (bundle: {path:?}): {msg}");
            }
        }
    }

    // ────────────────────────────────────────────────────────────────
    // Row D — software TCP-cksum bad (works regardless of hw offload)
    // ────────────────────────────────────────────────────────────────
    {
        let frame = build_minimal_tcp_frame(
            our_mac, PEER_IP, OUR_IP, src_port + 1, dst_port,
            /* corrupt_ip */ false, /* corrupt_tcp */ true,
        );
        let bucket = PressureBucket::open("pressure-counter-parity", "row_d_sw_tcp_bad", engine.counters());
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            for _ in 0..N_INJECT {
                engine.inject_rx_frame(&frame).expect("inject row D");
            }
            let after = common::pressure::CounterSnapshot::capture(engine.counters());
            let delta = after.delta_since(&bucket.before);
            assert_delta(&delta, "eth.rx_drop_cksum_bad", Relation::Eq(0));
            assert_delta(&delta, "ip.rx_csum_bad", Relation::Eq(0));
            assert_delta(&delta, "tcp.rx_bad_csum", Relation::Eq(N_INJECT as i64));
            assert_delta(&delta, "tcp.mbuf_refcnt_drop_unexpected", Relation::Eq(0));
            assert_delta(&delta, "obs.events_dropped", Relation::Eq(0));
        }));
        match result {
            Ok(()) => bucket.finish_ok(),
            Err(e) => {
                let msg = e.downcast_ref::<String>().cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "<non-string panic>".to_string());
                let path = bucket.finish_fail(engine.counters(), &cfg, vec![], msg.clone());
                panic!("Row D failed (bundle: {path:?}): {msg}");
            }
        }
    }

    // ────────────────────────────────────────────────────────────────
    // Row A — NIC-reported IP-cksum bad (hw-offload-rx-cksum path)
    // The NIC sets RTE_MBUF_F_RX_IP_CKSUM_BAD in ol_flags; the frame
    // bytes themselves are valid (correct checksums). When
    // hw-offload-rx-cksum is ON, the engine trusts the NIC flag:
    //   l3_ip.rs bumps eth.rx_drop_cksum_bad + returns L3Drop::CsumBad
    //   engine.rs handle_ipv4 bumps ip.rx_csum_bad
    // Post-B1 expectation: EXACTLY 1 bump per layer per frame.
    //
    // Runtime guard: TAP vdevs report no NIC offload capability so
    // rx_cksum_offload_active is false even when the compile-time feature
    // is on. Skip rows A/B rather than asserting all-zero deltas.
    // ────────────────────────────────────────────────────────────────
    #[cfg(feature = "hw-offload-rx-cksum")]
    if !engine.rx_cksum_offload_active() {
        eprintln!(
            "[pressure-counter-parity] rows A/B skipped: \
             rx_cksum_offload_active=false (TAP reports no NIC offload)"
        );
    } else {
        // Row A ─ NIC IP-BAD
        {
            let frame = build_minimal_tcp_frame(
                our_mac, PEER_IP, OUR_IP, src_port + 2, dst_port,
                /* corrupt_ip */ false, /* corrupt_tcp */ false,
            );
            let bucket = PressureBucket::open("pressure-counter-parity", "row_a_nic_ip_bad", engine.counters());
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                for _ in 0..N_INJECT {
                    engine.inject_rx_frame_with_ol_flags(&frame, RTE_MBUF_F_RX_IP_CKSUM_BAD)
                        .expect("inject row A");
                }
                let after = common::pressure::CounterSnapshot::capture(engine.counters());
                let delta = after.delta_since(&bucket.before);
                // Post-B1: single bump per layer per frame
                assert_delta(&delta, "eth.rx_drop_cksum_bad", Relation::Eq(N_INJECT as i64));
                assert_delta(&delta, "ip.rx_csum_bad", Relation::Eq(N_INJECT as i64));
                assert_delta(&delta, "tcp.rx_bad_csum", Relation::Eq(0));
                assert_delta(&delta, "tcp.mbuf_refcnt_drop_unexpected", Relation::Eq(0));
                assert_delta(&delta, "obs.events_dropped", Relation::Eq(0));
            }));
            match result {
                Ok(()) => bucket.finish_ok(),
                Err(e) => {
                    let msg = e.downcast_ref::<String>().cloned()
                        .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_else(|| "<non-string panic>".to_string());
                    let path = bucket.finish_fail(engine.counters(), &cfg, vec![], msg.clone());
                    panic!("Row A failed (bundle: {path:?}): {msg}");
                }
            }
        }

        // ────────────────────────────────────────────────────────────────
        // Row B — NIC-reported L4-cksum bad (hw-offload-rx-cksum path)
        // RTE_MBUF_F_RX_L4_CKSUM_BAD triggers the L4-BAD branch:
        //   engine.rs bumps eth.rx_drop_cksum_bad + tcp.rx_bad_csum
        // ────────────────────────────────────────────────────────────────
        {
            let frame = build_minimal_tcp_frame(
                our_mac, PEER_IP, OUR_IP, src_port + 3, dst_port,
                /* corrupt_ip */ false, /* corrupt_tcp */ false,
            );
            let bucket = PressureBucket::open("pressure-counter-parity", "row_b_nic_l4_bad", engine.counters());
            let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
                for _ in 0..N_INJECT {
                    engine.inject_rx_frame_with_ol_flags(&frame, RTE_MBUF_F_RX_L4_CKSUM_BAD)
                        .expect("inject row B");
                }
                let after = common::pressure::CounterSnapshot::capture(engine.counters());
                let delta = after.delta_since(&bucket.before);
                assert_delta(&delta, "eth.rx_drop_cksum_bad", Relation::Eq(N_INJECT as i64));
                assert_delta(&delta, "ip.rx_csum_bad", Relation::Eq(0));
                assert_delta(&delta, "tcp.rx_bad_csum", Relation::Eq(N_INJECT as i64));
                assert_delta(&delta, "tcp.mbuf_refcnt_drop_unexpected", Relation::Eq(0));
                assert_delta(&delta, "obs.events_dropped", Relation::Eq(0));
            }));
            match result {
                Ok(()) => bucket.finish_ok(),
                Err(e) => {
                    let msg = e.downcast_ref::<String>().cloned()
                        .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                        .unwrap_or_else(|| "<non-string panic>".to_string());
                    let path = bucket.finish_fail(engine.counters(), &cfg, vec![], msg.clone());
                    panic!("Row B failed (bundle: {path:?}): {msg}");
                }
            }
        }
    }

    eprintln!(
        "[pressure-counter-parity] all rows passed \
         (rows A/B only active with hw-offload-rx-cksum feature)"
    );
}
