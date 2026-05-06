//! Pressure Suite — `pressure-offload-matrix-completeness`.
//! A11.4 Lane E.
//!
//! Complements `pressure-counter-parity-offload-matrix` (T6, A11.1 Lane C)
//! which covered the BAD-path rows A–D.  This suite covers the GOOD-path
//! rows that verify the engine does NOT bump error counters when the NIC
//! signals a clean checksum — and that a GOOD flag suppresses software
//! re-verification even when the frame bytes contain a bad checksum.
//!
//! ## Two rows
//!
//! | Row | Path | Frame | ol_flags |
//! |-----|------|-------|---------|
//! | E | GOOD: both layers valid | valid IP + TCP cksum | IP_CKSUM_GOOD \| L4_CKSUM_GOOD |
//! | F | GOOD: L4 overrides bad TCP | valid IP, bad TCP cksum | L4_CKSUM_GOOD |
//!
//! ## Expected counter deltas per N_INJECT frames
//!
//! | Row | eth.rx_drop_cksum_bad | ip.rx_csum_bad | tcp.rx_bad_csum |
//! |-----|----------------------|----------------|-----------------|
//! | E   | 0                    | 0              | 0               |
//! | F   | 0                    | 0              | 0               |
//!
//! Row E verifies that GOOD offload flags do not accidentally bump any
//! error counter — guards against a "double-report" regression where a
//! future change might count a frame as bad even though the NIC said good.
//!
//! Row F verifies that `L4_CKSUM_GOOD` suppresses the software TCP
//! checksum verification path: a frame whose TCP checksum bytes are
//! intentionally corrupted still passes because the NIC declared the L4
//! clean.  Row F requires `hw-offload-rx-cksum` compiled in AND the
//! runtime `rx_cksum_offload_active` latch to be true; it is skipped on
//! TAP vdevs where the NIC does not advertise checksum-offload capability
//! (analogous to the skip applied to rows A/B in T6).
//!
//! ## TAP gate
//!
//! EAL initialisation requires `DPDK_NET_TEST_TAP=1` + `sudo`.
//! Row E runs on TAP (offload inactive; SW verifies the valid frame and
//! produces no errors, so the all-zero assertions still hold).
//! Row F is skipped when `rx_cksum_offload_active=false`.
//!
//! ## Feature gates
//!
//! * `pressure-test`        — `PressureBucket` / `CounterSnapshot` DSL.
//! * `test-inject`          — `Engine::inject_rx_frame_with_ol_flags`.
//! * `hw-offload-rx-cksum`  — Row F compile-gate.

#![cfg(all(feature = "pressure-test", feature = "test-inject"))]

mod common;
use common::pressure::{assert_delta, CounterSnapshot, PressureBucket, Relation};

use dpdk_net_core::engine::{eal_init, Engine, EngineConfig};

use std::panic::AssertUnwindSafe;
use std::process::Command;
use std::thread;
use std::time::Duration;

const TAP_IFACE: &str = "resdtap52";
const OUR_IP: u32 = 0x0a_63_34_02; // 10.99.52.2
const PEER_IP: u32 = 0x0a_63_34_01; // 10.99.52.1
const OUR_IP_STR: &str = "10.99.52.2";

/// Frames injected per row.
const N_INJECT: u64 = 100;

// DPDK RX GOOD offload flag constants (from dpdk_consts.rs).
const RTE_MBUF_F_RX_IP_CKSUM_GOOD: u64 = 1u64 << 7; // 0x0080
const RTE_MBUF_F_RX_L4_CKSUM_GOOD: u64 = 1u64 << 8; // 0x0100

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
    let _ = Command::new("ip").args(["link", "set", iface, "up"]).status();
    let _ = Command::new("ip")
        .args(["addr", "add", "10.99.52.1/24", "dev", iface])
        .status();
}

fn pin_arp(iface: &str, ip: &str, mac: &str) {
    let _ = Command::new("ip")
        .args(["neigh", "replace", ip, "lladdr", mac, "dev", iface, "nud", "permanent"])
        .status();
}

fn mac_hex(mac: [u8; 6]) -> String {
    format!(
        "{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

/// Internet checksum over `data` (one's-complement sum of 16-bit words).
fn internet_checksum(data: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < data.len() {
        sum += u16::from_be_bytes([data[i], data[i + 1]]) as u32;
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
/// Mirrors `build_minimal_tcp_frame` from `pressure_counter_parity.rs`.
/// `corrupt_ip_cksum` zeroes the IP checksum; `corrupt_tcp_cksum` zeroes
/// the TCP checksum.  Both false → both checksums are valid.
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

    // Ethernet header (14 bytes)
    frame.extend_from_slice(&dst_mac);
    frame.extend_from_slice(&[0x52, 0x54, 0x00, 0x12, 0x34, 0x56]); // src MAC
    frame.extend_from_slice(&[0x08, 0x00]); // ethertype = IPv4

    // IPv4 header (20 bytes)
    let tcp_total_len: u16 = 20 + 20; // TCP header, no payload
    let ip_total_len: u16 = 20 + tcp_total_len;
    let ip_hdr_no_cksum: [u8; 20] = [
        0x45, 0x00,
        (ip_total_len >> 8) as u8, ip_total_len as u8,
        0x00, 0x00,
        0x40, 0x00, // flags=DF, frag=0
        0x40, 0x06, // TTL=64, proto=TCP
        0x00, 0x00, // checksum placeholder
        ((src_ip >> 24) & 0xFF) as u8,
        ((src_ip >> 16) & 0xFF) as u8,
        ((src_ip >>  8) & 0xFF) as u8,
        ( src_ip        & 0xFF) as u8,
        ((dst_ip >> 24) & 0xFF) as u8,
        ((dst_ip >> 16) & 0xFF) as u8,
        ((dst_ip >>  8) & 0xFF) as u8,
        ( dst_ip        & 0xFF) as u8,
    ];
    let ip_cksum = internet_checksum(&ip_hdr_no_cksum);
    let mut ip_hdr = ip_hdr_no_cksum;
    if corrupt_ip_cksum {
        ip_hdr[10] = 0xFF;
        ip_hdr[11] = 0xFF;
    } else {
        ip_hdr[10] = (ip_cksum >> 8) as u8;
        ip_hdr[11] =  ip_cksum       as u8;
    }
    frame.extend_from_slice(&ip_hdr);

    // TCP header (20 bytes)
    let tcp_hdr_no_cksum: [u8; 20] = [
        (src_port >> 8) as u8, src_port as u8,
        (dst_port >> 8) as u8, dst_port as u8,
        0x00, 0x00, 0x00, 0x01, // seq = 1
        0x00, 0x00, 0x00, 0x00, // ack = 0
        0x50,                   // data offset = 5 (20 bytes)
        0x10,                   // flags: ACK
        0x04, 0x00,             // window = 1024
        0x00, 0x00,             // checksum placeholder
        0x00, 0x00,             // urgent pointer
    ];
    let pseudo: Vec<u8> = {
        let mut p = Vec::with_capacity(12 + 20);
        p.extend_from_slice(&ip_hdr_no_cksum[12..16]); // src IP
        p.extend_from_slice(&ip_hdr_no_cksum[16..20]); // dst IP
        p.push(0x00);
        p.push(0x06); // proto = TCP
        p.push((tcp_total_len >> 8) as u8);
        p.push( tcp_total_len       as u8);
        p.extend_from_slice(&tcp_hdr_no_cksum);
        p
    };
    let tcp_cksum = internet_checksum(&pseudo);
    let mut tcp_hdr = tcp_hdr_no_cksum;
    if corrupt_tcp_cksum {
        tcp_hdr[16] = 0xFF;
        tcp_hdr[17] = 0xFF;
    } else {
        tcp_hdr[16] = (tcp_cksum >> 8) as u8;
        tcp_hdr[17] =  tcp_cksum       as u8;
    }
    frame.extend_from_slice(&tcp_hdr);

    frame
}

#[test]
fn pressure_offload_matrix_completeness() {
    if skip_if_not_tap() {
        return;
    }

    let args = [
        "dpdk-net-pressure-offload-matrix-completeness",
        "--in-memory",
        "--no-pci",
        "--vdev=net_tap0,iface=resdtap52",
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

    // Synthetic ports — no active connection needed; checksum processing
    // occurs before the flow-table lookup.
    let src_port: u16 = 55_555;
    let dst_port: u16 = 9_999;

    // ────────────────────────────────────────────────────────────────────
    // Row E — NIC GOOD path: valid frame + IP_CKSUM_GOOD | L4_CKSUM_GOOD.
    //
    // When hw-offload-rx-cksum is active: GOOD flags → CksumOutcome::Good
    // → no error counters.  When offload is inactive (TAP vdev):
    // the engine falls back to SW verification; the frame has valid
    // checksums so SW also passes → no error counters.  Assertions hold
    // in both code paths — Row E is not skip-gated.
    // ────────────────────────────────────────────────────────────────────
    {
        let frame = build_minimal_tcp_frame(
            our_mac, PEER_IP, OUR_IP, src_port, dst_port,
            false, false, // both checksums valid
        );
        let bucket = PressureBucket::open(
            "pressure-offload-matrix-completeness",
            "row_e_nic_good_both",
            engine.counters(),
        );
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            for _ in 0..N_INJECT {
                engine
                    .inject_rx_frame_with_ol_flags(
                        &frame,
                        RTE_MBUF_F_RX_IP_CKSUM_GOOD | RTE_MBUF_F_RX_L4_CKSUM_GOOD,
                    )
                    .expect("inject row E");
            }
            let after = CounterSnapshot::capture(engine.counters());
            let delta = after.delta_since(&bucket.before);
            assert_delta(&delta, "eth.rx_drop_cksum_bad",           Relation::Eq(0));
            assert_delta(&delta, "ip.rx_csum_bad",                  Relation::Eq(0));
            assert_delta(&delta, "tcp.rx_bad_csum",                 Relation::Eq(0));
            assert_delta(&delta, "tcp.mbuf_refcnt_drop_unexpected", Relation::Eq(0));
            assert_delta(&delta, "obs.events_dropped",              Relation::Eq(0));
        }));
        match result {
            Ok(()) => bucket.finish_ok(),
            Err(e) => {
                let msg = e.downcast_ref::<String>().cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "<non-string panic>".to_string());
                let path = bucket.finish_fail(engine.counters(), &cfg, vec![], msg.clone());
                panic!("Row E failed (bundle: {path:?}): {msg}");
            }
        }
    }

    // ────────────────────────────────────────────────────────────────────
    // Row F — L4_CKSUM_GOOD suppresses SW TCP-cksum verification.
    //
    // The frame carries an intentionally bad TCP checksum.  When the
    // offload latch is active the engine reads L4_CKSUM_GOOD and skips
    // software re-verification → tcp.rx_bad_csum stays zero.  When
    // offload is inactive (TAP vdev), the engine falls to SW which would
    // catch the bad checksum; the row is skipped rather than letting it
    // produce a false failure (mirrors rows A/B skip in T6).
    // ────────────────────────────────────────────────────────────────────
    #[cfg(feature = "hw-offload-rx-cksum")]
    if !engine.rx_cksum_offload_active() {
        eprintln!(
            "[pressure-offload-matrix-completeness] row F skipped: \
             rx_cksum_offload_active=false (TAP reports no NIC offload)"
        );
    } else {
        let frame = build_minimal_tcp_frame(
            our_mac, PEER_IP, OUR_IP, src_port + 1, dst_port,
            false, true, // valid IP cksum, intentionally bad TCP cksum
        );
        let bucket = PressureBucket::open(
            "pressure-offload-matrix-completeness",
            "row_f_nic_l4_good_bad_bytes",
            engine.counters(),
        );
        let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
            for _ in 0..N_INJECT {
                engine
                    .inject_rx_frame_with_ol_flags(&frame, RTE_MBUF_F_RX_L4_CKSUM_GOOD)
                    .expect("inject row F");
            }
            let after = CounterSnapshot::capture(engine.counters());
            let delta = after.delta_since(&bucket.before);
            assert_delta(&delta, "eth.rx_drop_cksum_bad",           Relation::Eq(0));
            assert_delta(&delta, "ip.rx_csum_bad",                  Relation::Eq(0));
            assert_delta(&delta, "tcp.rx_bad_csum",                 Relation::Eq(0));
            assert_delta(&delta, "tcp.mbuf_refcnt_drop_unexpected", Relation::Eq(0));
            assert_delta(&delta, "obs.events_dropped",              Relation::Eq(0));
        }));
        match result {
            Ok(()) => bucket.finish_ok(),
            Err(e) => {
                let msg = e.downcast_ref::<String>().cloned()
                    .or_else(|| e.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_else(|| "<non-string panic>".to_string());
                let path = bucket.finish_fail(engine.counters(), &cfg, vec![], msg.clone());
                panic!("Row F failed (bundle: {path:?}): {msg}");
            }
        }
    }

    eprintln!(
        "[pressure-offload-matrix-completeness] complete \
         (row F active only with hw-offload-rx-cksum + rx_cksum_offload_active)"
    );
}
