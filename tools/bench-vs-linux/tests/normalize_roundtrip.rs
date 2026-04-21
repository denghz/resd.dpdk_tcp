//! A10 Plan B Task 9 — divergence-normalisation round-trip tests.
//!
//! Build two synthetic pcap streams with different random ISS + TSval
//! bases but otherwise identical wire behaviour, canonicalise both,
//! then byte-compare. The equal-after-normalisation invariant is the
//! whole contract for mode B — if it fails here, the wire-diff runner
//! will produce false positives and operators can't trust the tool.
//!
//! The synthetic pcap builder lives inline in this file rather than
//! as committed fixtures because generating them programmatically
//! keeps the test self-contained: a Scapy dependency would ship
//! opaque `.pcap` blobs that future me can't debug without running
//! Scapy too.
//!
//! # Test matrix
//!
//! - `canonicalize_produces_identical_bytes_for_same_flow` — two SYN/
//!   SYN-ACK/ACK streams with different random ISS + TSval should
//!   canonicalise to identical bytes.
//! - `canonicalize_survives_missing_ts_option` — no-TSopt streams
//!   still normalise (just seq/ack + MAC).
//! - `canonicalize_survives_window_scale_option` — window-scale
//!   option present should pass through unchanged and still match.
//! - `canonicalize_handles_sack_blocks` — SACK blocks must rewrite
//!   with the reverse direction's ISS pin.
//! - `canonicalize_handles_fin` — FIN doesn't need special rewrites
//!   but should still canonicalise cleanly (smoke test the flag
//!   path).
//! - `canonicalize_passes_through_non_ipv4` — ARP / other ethertypes
//!   pass through unchanged; the byte diff is exact on those bytes.

use std::io::Cursor;
use std::time::Duration;

use bench_vs_linux::normalize::{byte_diff_count, canonicalize_pcap, CanonicalizationOptions};
use pcap_file::pcap::{PcapPacket, PcapWriter};

/// A single synthetic packet used by the test pcap builder.
#[derive(Clone)]
struct SynthPacket {
    eth_src: [u8; 6],
    eth_dst: [u8; 6],
    ethertype: u16,
    /// `None` for non-IPv4 frames — builder emits ethertype + opaque
    /// payload.
    ipv4: Option<SynthIpv4>,
    /// For non-IPv4 frames: the raw payload past ethertype.
    raw_payload: Vec<u8>,
}

#[derive(Clone)]
struct SynthIpv4 {
    src_ip: [u8; 4],
    dst_ip: [u8; 4],
    ip_id: u16,
    proto: u8,
    /// TCP segment payload (may be empty). Absent fields imply a
    /// pure-TCP-header-only segment.
    tcp: Option<SynthTcp>,
}

#[derive(Clone)]
struct SynthTcp {
    src_port: u16,
    dst_port: u16,
    seq: u32,
    ack: u32,
    /// Data offset in bytes (20 = no options, 32 = +TSopt, etc). Must
    /// be a multiple of 4 and >= 20.
    data_offset: u8,
    flags: u8,
    window: u16,
    /// Options bytes (NOP-padded to a multiple of 4, caller's job).
    options: Vec<u8>,
    payload: Vec<u8>,
}

const TCP_FLAG_SYN: u8 = 0x02;
const TCP_FLAG_ACK: u8 = 0x10;
const TCP_FLAG_FIN: u8 = 0x01;

fn build_pcap(packets: &[SynthPacket]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let mut wr = PcapWriter::new(&mut buf).expect("PcapWriter::new");
        for (i, p) in packets.iter().enumerate() {
            let bytes = serialize_frame(p);
            let ts = Duration::from_nanos(1_000_000 * (i as u64 + 1));
            let pkt = PcapPacket::new_owned(ts, bytes.len() as u32, bytes);
            wr.write_packet(&pkt).expect("write_packet");
        }
    }
    buf
}

fn serialize_frame(p: &SynthPacket) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&p.eth_dst);
    out.extend_from_slice(&p.eth_src);
    out.extend_from_slice(&p.ethertype.to_be_bytes());
    if let Some(ip) = &p.ipv4 {
        let tcp_bytes = ip.tcp.as_ref().map(serialize_tcp).unwrap_or_default();
        let total_len = 20 + tcp_bytes.len();
        let mut ip_hdr = [0u8; 20];
        ip_hdr[0] = 0x45; // Version 4, IHL 5
        ip_hdr[1] = 0; // DSCP/ECN
        ip_hdr[2..4].copy_from_slice(&(total_len as u16).to_be_bytes());
        ip_hdr[4..6].copy_from_slice(&ip.ip_id.to_be_bytes());
        ip_hdr[6..8].copy_from_slice(&0u16.to_be_bytes());
        ip_hdr[8] = 64; // TTL
        ip_hdr[9] = ip.proto;
        ip_hdr[10..12].copy_from_slice(&0u16.to_be_bytes()); // csum placeholder
        ip_hdr[12..16].copy_from_slice(&ip.src_ip);
        ip_hdr[16..20].copy_from_slice(&ip.dst_ip);
        let csum = rfc1071(&ip_hdr);
        ip_hdr[10..12].copy_from_slice(&csum.to_be_bytes());
        out.extend_from_slice(&ip_hdr);
        // TCP payload is appended; the TCP checksum needs the pseudo-
        // header so we recompute it here over the already-serialised
        // TCP bytes.
        if !tcp_bytes.is_empty() {
            let mut tcp_bytes = tcp_bytes;
            // Zero the checksum field (offset 16 within TCP header).
            tcp_bytes[16] = 0;
            tcp_bytes[17] = 0;
            let csum = synth_tcp_checksum(&ip.src_ip, &ip.dst_ip, &tcp_bytes);
            tcp_bytes[16..18].copy_from_slice(&csum.to_be_bytes());
            out.extend_from_slice(&tcp_bytes);
        }
    } else {
        out.extend_from_slice(&p.raw_payload);
    }
    out
}

fn serialize_tcp(t: &SynthTcp) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&t.src_port.to_be_bytes());
    buf.extend_from_slice(&t.dst_port.to_be_bytes());
    buf.extend_from_slice(&t.seq.to_be_bytes());
    buf.extend_from_slice(&t.ack.to_be_bytes());
    buf.push((t.data_offset / 4) << 4); // data_offset in 4-byte words, in high nibble
    buf.push(t.flags);
    buf.extend_from_slice(&t.window.to_be_bytes());
    buf.extend_from_slice(&0u16.to_be_bytes()); // checksum placeholder
    buf.extend_from_slice(&0u16.to_be_bytes()); // urg ptr
    buf.extend_from_slice(&t.options);
    buf.extend_from_slice(&t.payload);
    buf
}

fn rfc1071(bytes: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < bytes.len() {
        sum = sum.wrapping_add(u32::from(u16::from_be_bytes([bytes[i], bytes[i + 1]])));
        i += 2;
    }
    if i < bytes.len() {
        sum = sum.wrapping_add(u32::from(u16::from_be_bytes([bytes[i], 0])));
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

fn synth_tcp_checksum(src_ip: &[u8; 4], dst_ip: &[u8; 4], tcp: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    sum += u32::from(u16::from_be_bytes([src_ip[0], src_ip[1]]));
    sum += u32::from(u16::from_be_bytes([src_ip[2], src_ip[3]]));
    sum += u32::from(u16::from_be_bytes([dst_ip[0], dst_ip[1]]));
    sum += u32::from(u16::from_be_bytes([dst_ip[2], dst_ip[3]]));
    sum += 6u32; // proto TCP
    sum += tcp.len() as u32;
    let mut i = 0;
    while i + 1 < tcp.len() {
        sum = sum.wrapping_add(u32::from(u16::from_be_bytes([tcp[i], tcp[i + 1]])));
        i += 2;
    }
    if i < tcp.len() {
        sum = sum.wrapping_add(u32::from(u16::from_be_bytes([tcp[i], 0])));
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

// ---------------------------------------------------------------------------
// Helpers to build typical 3-way handshake flows with parameters
// (ISS, TSval, MAC) as inputs so we can vary them across two pcaps.
// ---------------------------------------------------------------------------

fn mac(v: u8) -> [u8; 6] {
    [0x00, 0x11, 0x22, 0x33, 0x44, v]
}

/// Build a 3-packet handshake (SYN, SYN-ACK, ACK) with TSopt. Returns
/// the pcap bytes.
///
/// `a_*` = active opener side (src_ip 10.0.0.1), `b_*` = passive
/// acceptor side (src_ip 10.0.0.2). Ports are fixed.
#[allow(clippy::too_many_arguments)]
fn build_handshake_with_tsopt(
    a_mac: [u8; 6],
    b_mac: [u8; 6],
    a_iss: u32,
    b_iss: u32,
    a_tsval: u32,
    b_tsval: u32,
    include_ts_option: bool,
) -> Vec<u8> {
    let a_ip = [10, 0, 0, 1];
    let b_ip = [10, 0, 0, 2];
    let a_port: u16 = 40000;
    let b_port: u16 = 10001;
    // TSopt: kind=8 len=10 TSval(4) TSecr(4) + 2 NOPs for 4-byte
    // alignment. 12 bytes options total; data_offset = 20 + 12 = 32.
    let mk_tsopt = |tsval: u32, tsecr: u32| -> Vec<u8> {
        // NOP, NOP, Timestamps (kind=8), length=10, TSval(4), TSecr(4)
        // — 12 bytes total, 4-byte aligned for the 32-bit TCP option
        // word.
        let mut o = vec![1u8, 1, 8, 10];
        o.extend_from_slice(&tsval.to_be_bytes());
        o.extend_from_slice(&tsecr.to_be_bytes());
        o
    };
    let (opts_syn, opts_synack, opts_ack, d_off) = if include_ts_option {
        (
            mk_tsopt(a_tsval, 0),
            mk_tsopt(b_tsval, a_tsval),
            mk_tsopt(a_tsval.wrapping_add(1), b_tsval),
            32u8,
        )
    } else {
        (Vec::new(), Vec::new(), Vec::new(), 20u8)
    };
    let syn = SynthPacket {
        eth_src: a_mac,
        eth_dst: b_mac,
        ethertype: 0x0800,
        ipv4: Some(SynthIpv4 {
            src_ip: a_ip,
            dst_ip: b_ip,
            ip_id: 0,
            proto: 6,
            tcp: Some(SynthTcp {
                src_port: a_port,
                dst_port: b_port,
                seq: a_iss,
                ack: 0,
                data_offset: d_off,
                flags: TCP_FLAG_SYN,
                window: 64240,
                options: opts_syn,
                payload: Vec::new(),
            }),
        }),
        raw_payload: Vec::new(),
    };
    let synack = SynthPacket {
        eth_src: b_mac,
        eth_dst: a_mac,
        ethertype: 0x0800,
        ipv4: Some(SynthIpv4 {
            src_ip: b_ip,
            dst_ip: a_ip,
            ip_id: 0,
            proto: 6,
            tcp: Some(SynthTcp {
                src_port: b_port,
                dst_port: a_port,
                seq: b_iss,
                ack: a_iss.wrapping_add(1),
                data_offset: d_off,
                flags: TCP_FLAG_SYN | TCP_FLAG_ACK,
                window: 64240,
                options: opts_synack,
                payload: Vec::new(),
            }),
        }),
        raw_payload: Vec::new(),
    };
    let ack = SynthPacket {
        eth_src: a_mac,
        eth_dst: b_mac,
        ethertype: 0x0800,
        ipv4: Some(SynthIpv4 {
            src_ip: a_ip,
            dst_ip: b_ip,
            ip_id: 0,
            proto: 6,
            tcp: Some(SynthTcp {
                src_port: a_port,
                dst_port: b_port,
                seq: a_iss.wrapping_add(1),
                ack: b_iss.wrapping_add(1),
                data_offset: d_off,
                flags: TCP_FLAG_ACK,
                window: 64240,
                options: opts_ack,
                payload: Vec::new(),
            }),
        }),
        raw_payload: Vec::new(),
    };
    build_pcap(&[syn, synack, ack])
}

#[test]
fn canonicalize_produces_identical_bytes_for_same_flow() {
    // Two pcaps with wildly different ISS + TSval but the same wire
    // behaviour. Canonicalisation should collapse them to identical
    // bytes.
    let pcap_a = build_handshake_with_tsopt(mac(0x01), mac(0x02), 0xDEAD_BEEF, 0xFACE_F00D, 100, 200, true);
    let pcap_b = build_handshake_with_tsopt(mac(0xAA), mac(0xBB), 0x0000_0007, 0x9000_0000, 42, 77, true);
    let opts = CanonicalizationOptions::default();
    let can_a = canonicalize_pcap(&pcap_a, &opts).expect("canonicalise A");
    let can_b = canonicalize_pcap(&pcap_b, &opts).expect("canonicalise B");
    assert_eq!(
        byte_diff_count(&can_a, &can_b),
        0,
        "canonicalised byte streams must match"
    );
    assert_eq!(can_a, can_b);
}

#[test]
fn canonicalize_survives_missing_ts_option() {
    let pcap_a = build_handshake_with_tsopt(mac(0x01), mac(0x02), 0xDEAD_BEEF, 0xFACE_F00D, 0, 0, false);
    let pcap_b = build_handshake_with_tsopt(mac(0xAA), mac(0xBB), 0x0000_0007, 0x9000_0000, 0, 0, false);
    let opts = CanonicalizationOptions::default();
    let can_a = canonicalize_pcap(&pcap_a, &opts).expect("canonicalise A");
    let can_b = canonicalize_pcap(&pcap_b, &opts).expect("canonicalise B");
    assert_eq!(can_a, can_b);
}

#[test]
fn canonicalize_survives_window_scale_option() {
    // Window-scale option (kind=3 len=3 shift=7) + NOP padding.
    // We build this manually because the helper above only emits TSopt.
    let mk_wscale_syn = |src_mac: [u8; 6], dst_mac: [u8; 6], seq: u32| SynthPacket {
        eth_src: src_mac,
        eth_dst: dst_mac,
        ethertype: 0x0800,
        ipv4: Some(SynthIpv4 {
            src_ip: [10, 0, 0, 1],
            dst_ip: [10, 0, 0, 2],
            ip_id: 0,
            proto: 6,
            tcp: Some(SynthTcp {
                src_port: 40000,
                dst_port: 10001,
                seq,
                ack: 0,
                data_offset: 24, // 20 + 4 bytes options
                flags: TCP_FLAG_SYN,
                window: 64240,
                options: vec![1, 3, 3, 7], // NOP, WS(3), len(3), shift(7)
                payload: Vec::new(),
            }),
        }),
        raw_payload: Vec::new(),
    };
    let pcap_a = build_pcap(&[mk_wscale_syn(mac(0x01), mac(0x02), 0xDEAD_BEEF)]);
    let pcap_b = build_pcap(&[mk_wscale_syn(mac(0xAA), mac(0xBB), 0x0000_0007)]);
    let opts = CanonicalizationOptions::default();
    let can_a = canonicalize_pcap(&pcap_a, &opts).expect("canonicalise A");
    let can_b = canonicalize_pcap(&pcap_b, &opts).expect("canonicalise B");
    assert_eq!(can_a, can_b);
}

#[test]
fn canonicalize_handles_sack_blocks() {
    // After the SYN-ACK-ACK handshake, the passive side emits an ACK
    // with a SACK block (kind=5 len=10 = one block of 8 bytes) covering
    // (a_iss+100, a_iss+200) in the active side's seq space.
    let a_iss_in_a = 0x1000_0000u32;
    let a_iss_in_b = 0x7000_0000u32;
    let b_iss_in_a = 0x2000_0000u32;
    let b_iss_in_b = 0x3000_0000u32;

    let make_flow = |a_iss: u32, b_iss: u32, a_mac: [u8; 6], b_mac: [u8; 6]| -> Vec<u8> {
        let a_ip = [10, 0, 0, 1];
        let b_ip = [10, 0, 0, 2];
        let a_port: u16 = 40000;
        let b_port: u16 = 10001;
        // Handshake.
        let syn = SynthPacket {
            eth_src: a_mac,
            eth_dst: b_mac,
            ethertype: 0x0800,
            ipv4: Some(SynthIpv4 {
                src_ip: a_ip,
                dst_ip: b_ip,
                ip_id: 0,
                proto: 6,
                tcp: Some(SynthTcp {
                    src_port: a_port,
                    dst_port: b_port,
                    seq: a_iss,
                    ack: 0,
                    data_offset: 20,
                    flags: TCP_FLAG_SYN,
                    window: 64240,
                    options: Vec::new(),
                    payload: Vec::new(),
                }),
            }),
            raw_payload: Vec::new(),
        };
        let synack = SynthPacket {
            eth_src: b_mac,
            eth_dst: a_mac,
            ethertype: 0x0800,
            ipv4: Some(SynthIpv4 {
                src_ip: b_ip,
                dst_ip: a_ip,
                ip_id: 0,
                proto: 6,
                tcp: Some(SynthTcp {
                    src_port: b_port,
                    dst_port: a_port,
                    seq: b_iss,
                    ack: a_iss.wrapping_add(1),
                    data_offset: 20,
                    flags: TCP_FLAG_SYN | TCP_FLAG_ACK,
                    window: 64240,
                    options: Vec::new(),
                    payload: Vec::new(),
                }),
            }),
            raw_payload: Vec::new(),
        };
        // NOP, NOP, SACK (kind=5), length=10, one 8-byte block.
        let mut sack_opts = vec![1u8, 1, 5, 10];
        let left = a_iss.wrapping_add(100);
        let right = a_iss.wrapping_add(200);
        sack_opts.extend_from_slice(&left.to_be_bytes());
        sack_opts.extend_from_slice(&right.to_be_bytes());
        let sack_ack = SynthPacket {
            eth_src: b_mac,
            eth_dst: a_mac,
            ethertype: 0x0800,
            ipv4: Some(SynthIpv4 {
                src_ip: b_ip,
                dst_ip: a_ip,
                ip_id: 0,
                proto: 6,
                tcp: Some(SynthTcp {
                    src_port: b_port,
                    dst_port: a_port,
                    seq: b_iss.wrapping_add(1),
                    ack: a_iss.wrapping_add(1),
                    data_offset: 32, // 20 + 12 bytes options
                    flags: TCP_FLAG_ACK,
                    window: 64240,
                    options: sack_opts,
                    payload: Vec::new(),
                }),
            }),
            raw_payload: Vec::new(),
        };
        build_pcap(&[syn, synack, sack_ack])
    };
    let pcap_a = make_flow(a_iss_in_a, b_iss_in_a, mac(0x01), mac(0x02));
    let pcap_b = make_flow(a_iss_in_b, b_iss_in_b, mac(0xAA), mac(0xBB));
    let opts = CanonicalizationOptions::default();
    let can_a = canonicalize_pcap(&pcap_a, &opts).expect("canonicalise A");
    let can_b = canonicalize_pcap(&pcap_b, &opts).expect("canonicalise B");
    assert_eq!(can_a, can_b);
}

#[test]
fn canonicalize_handles_fin() {
    // SYN / SYN-ACK / ACK / FIN-ACK / FIN-ACK / ACK full close.
    // We exercise the FIN path to make sure flag-only packets still
    // canonicalise without tripping the "no ACK flag" guard.
    let flow = |a_iss: u32, b_iss: u32, a_mac: [u8; 6], b_mac: [u8; 6]| -> Vec<u8> {
        let a_ip = [10, 0, 0, 1];
        let b_ip = [10, 0, 0, 2];
        let a_port: u16 = 40000;
        let b_port: u16 = 10001;
        let syn = SynthPacket {
            eth_src: a_mac,
            eth_dst: b_mac,
            ethertype: 0x0800,
            ipv4: Some(SynthIpv4 {
                src_ip: a_ip,
                dst_ip: b_ip,
                ip_id: 0,
                proto: 6,
                tcp: Some(SynthTcp {
                    src_port: a_port,
                    dst_port: b_port,
                    seq: a_iss,
                    ack: 0,
                    data_offset: 20,
                    flags: TCP_FLAG_SYN,
                    window: 64240,
                    options: Vec::new(),
                    payload: Vec::new(),
                }),
            }),
            raw_payload: Vec::new(),
        };
        let synack = SynthPacket {
            eth_src: b_mac,
            eth_dst: a_mac,
            ethertype: 0x0800,
            ipv4: Some(SynthIpv4 {
                src_ip: b_ip,
                dst_ip: a_ip,
                ip_id: 0,
                proto: 6,
                tcp: Some(SynthTcp {
                    src_port: b_port,
                    dst_port: a_port,
                    seq: b_iss,
                    ack: a_iss.wrapping_add(1),
                    data_offset: 20,
                    flags: TCP_FLAG_SYN | TCP_FLAG_ACK,
                    window: 64240,
                    options: Vec::new(),
                    payload: Vec::new(),
                }),
            }),
            raw_payload: Vec::new(),
        };
        let ack = SynthPacket {
            eth_src: a_mac,
            eth_dst: b_mac,
            ethertype: 0x0800,
            ipv4: Some(SynthIpv4 {
                src_ip: a_ip,
                dst_ip: b_ip,
                ip_id: 0,
                proto: 6,
                tcp: Some(SynthTcp {
                    src_port: a_port,
                    dst_port: b_port,
                    seq: a_iss.wrapping_add(1),
                    ack: b_iss.wrapping_add(1),
                    data_offset: 20,
                    flags: TCP_FLAG_ACK,
                    window: 64240,
                    options: Vec::new(),
                    payload: Vec::new(),
                }),
            }),
            raw_payload: Vec::new(),
        };
        let fin_a = SynthPacket {
            eth_src: a_mac,
            eth_dst: b_mac,
            ethertype: 0x0800,
            ipv4: Some(SynthIpv4 {
                src_ip: a_ip,
                dst_ip: b_ip,
                ip_id: 0,
                proto: 6,
                tcp: Some(SynthTcp {
                    src_port: a_port,
                    dst_port: b_port,
                    seq: a_iss.wrapping_add(1),
                    ack: b_iss.wrapping_add(1),
                    data_offset: 20,
                    flags: TCP_FLAG_FIN | TCP_FLAG_ACK,
                    window: 64240,
                    options: Vec::new(),
                    payload: Vec::new(),
                }),
            }),
            raw_payload: Vec::new(),
        };
        let fin_b = SynthPacket {
            eth_src: b_mac,
            eth_dst: a_mac,
            ethertype: 0x0800,
            ipv4: Some(SynthIpv4 {
                src_ip: b_ip,
                dst_ip: a_ip,
                ip_id: 0,
                proto: 6,
                tcp: Some(SynthTcp {
                    src_port: b_port,
                    dst_port: a_port,
                    seq: b_iss.wrapping_add(1),
                    ack: a_iss.wrapping_add(2),
                    data_offset: 20,
                    flags: TCP_FLAG_FIN | TCP_FLAG_ACK,
                    window: 64240,
                    options: Vec::new(),
                    payload: Vec::new(),
                }),
            }),
            raw_payload: Vec::new(),
        };
        let final_ack = SynthPacket {
            eth_src: a_mac,
            eth_dst: b_mac,
            ethertype: 0x0800,
            ipv4: Some(SynthIpv4 {
                src_ip: a_ip,
                dst_ip: b_ip,
                ip_id: 0,
                proto: 6,
                tcp: Some(SynthTcp {
                    src_port: a_port,
                    dst_port: b_port,
                    seq: a_iss.wrapping_add(2),
                    ack: b_iss.wrapping_add(2),
                    data_offset: 20,
                    flags: TCP_FLAG_ACK,
                    window: 64240,
                    options: Vec::new(),
                    payload: Vec::new(),
                }),
            }),
            raw_payload: Vec::new(),
        };
        build_pcap(&[syn, synack, ack, fin_a, fin_b, final_ack])
    };
    let pcap_a = flow(0xDEAD_BEEF, 0xFACE_F00D, mac(0x01), mac(0x02));
    let pcap_b = flow(0x0000_0007, 0x9000_0000, mac(0xAA), mac(0xBB));
    let opts = CanonicalizationOptions::default();
    let can_a = canonicalize_pcap(&pcap_a, &opts).expect("canonicalise A");
    let can_b = canonicalize_pcap(&pcap_b, &opts).expect("canonicalise B");
    assert_eq!(can_a, can_b, "FIN-carrying flows must canonicalise identically");
}

#[test]
fn canonicalize_passes_through_non_ipv4() {
    // Emit an ARP frame (ethertype 0x0806) + a plain TCP packet.
    // The ARP frame should pass through unchanged in both captures;
    // the TCP packet should still canonicalise.
    let arp = |src_mac: [u8; 6], dst_mac: [u8; 6]| SynthPacket {
        eth_src: src_mac,
        eth_dst: dst_mac,
        ethertype: 0x0806,
        ipv4: None,
        raw_payload: vec![0u8; 28], // fake ARP body
    };
    let pcap_a = build_pcap(&[
        arp(mac(0x01), mac(0x02)),
        // Follow with a SYN so the canonicalisation still has something
        // to pin.
        SynthPacket {
            eth_src: mac(0x01),
            eth_dst: mac(0x02),
            ethertype: 0x0800,
            ipv4: Some(SynthIpv4 {
                src_ip: [10, 0, 0, 1],
                dst_ip: [10, 0, 0, 2],
                ip_id: 0,
                proto: 6,
                tcp: Some(SynthTcp {
                    src_port: 40000,
                    dst_port: 10001,
                    seq: 0xDEAD_BEEF,
                    ack: 0,
                    data_offset: 20,
                    flags: TCP_FLAG_SYN,
                    window: 64240,
                    options: Vec::new(),
                    payload: Vec::new(),
                }),
            }),
            raw_payload: Vec::new(),
        },
    ]);
    let pcap_b = build_pcap(&[
        // Same ARP bytes (including same MACs — we want to verify the
        // ARP frame passes through unchanged)
        arp(mac(0x01), mac(0x02)),
        SynthPacket {
            eth_src: mac(0xAA),
            eth_dst: mac(0xBB),
            ethertype: 0x0800,
            ipv4: Some(SynthIpv4 {
                src_ip: [10, 0, 0, 1],
                dst_ip: [10, 0, 0, 2],
                ip_id: 0,
                proto: 6,
                tcp: Some(SynthTcp {
                    src_port: 40000,
                    dst_port: 10001,
                    seq: 0x0000_0007,
                    ack: 0,
                    data_offset: 20,
                    flags: TCP_FLAG_SYN,
                    window: 64240,
                    options: Vec::new(),
                    payload: Vec::new(),
                }),
            }),
            raw_payload: Vec::new(),
        },
    ]);
    let opts = CanonicalizationOptions::default();
    let can_a = canonicalize_pcap(&pcap_a, &opts).expect("canonicalise A");
    let can_b = canonicalize_pcap(&pcap_b, &opts).expect("canonicalise B");
    // The MACs of the ARP frame differ between the two pcaps only if
    // they were built with different inputs — they're identical here,
    // so byte-for-byte the ARP regions must match. The TCP SYN region
    // is normalised, so it must also match.
    assert_eq!(can_a, can_b);
}

#[test]
fn canonicalize_mid_stream_capture_pins_first_seen_seq() {
    // No SYN observed — the first packet seen on each direction
    // pins ISS = seq at that packet. The two captures start mid-stream
    // with identical seq offsets post-pin, so they still canonicalise
    // to the same bytes.
    let mid = |a_seq: u32, b_seq: u32, a_mac: [u8; 6], b_mac: [u8; 6]| -> Vec<u8> {
        let a_ip = [10, 0, 0, 1];
        let b_ip = [10, 0, 0, 2];
        // First packet: side A sends ACK with data.
        let p1 = SynthPacket {
            eth_src: a_mac,
            eth_dst: b_mac,
            ethertype: 0x0800,
            ipv4: Some(SynthIpv4 {
                src_ip: a_ip,
                dst_ip: b_ip,
                ip_id: 0,
                proto: 6,
                tcp: Some(SynthTcp {
                    src_port: 40000,
                    dst_port: 10001,
                    seq: a_seq,
                    ack: b_seq,
                    data_offset: 20,
                    flags: TCP_FLAG_ACK,
                    window: 64240,
                    options: Vec::new(),
                    payload: vec![0x11, 0x22, 0x33, 0x44],
                }),
            }),
            raw_payload: Vec::new(),
        };
        // Second packet: side B ACKs with seq + data.
        let p2 = SynthPacket {
            eth_src: b_mac,
            eth_dst: a_mac,
            ethertype: 0x0800,
            ipv4: Some(SynthIpv4 {
                src_ip: b_ip,
                dst_ip: a_ip,
                ip_id: 0,
                proto: 6,
                tcp: Some(SynthTcp {
                    src_port: 10001,
                    dst_port: 40000,
                    seq: b_seq,
                    ack: a_seq.wrapping_add(4),
                    data_offset: 20,
                    flags: TCP_FLAG_ACK,
                    window: 64240,
                    options: Vec::new(),
                    payload: Vec::new(),
                }),
            }),
            raw_payload: Vec::new(),
        };
        build_pcap(&[p1, p2])
    };
    let pcap_a = mid(0x5000_0000, 0x6000_0000, mac(0x01), mac(0x02));
    let pcap_b = mid(0xA000_0000, 0xB000_0000, mac(0xAA), mac(0xBB));
    let opts = CanonicalizationOptions::default();
    let can_a = canonicalize_pcap(&pcap_a, &opts).expect("canonicalise A");
    let can_b = canonicalize_pcap(&pcap_b, &opts).expect("canonicalise B");
    assert_eq!(can_a, can_b);
}

#[test]
fn canonicalize_diverging_behavior_shows_diff() {
    // Negative control: if the two streams diverge in real wire
    // behaviour (different payload bytes), canonicalisation must not
    // hide that.
    let flow = |a_iss: u32, payload: &[u8]| -> Vec<u8> {
        let a_mac = mac(0x01);
        let b_mac = mac(0x02);
        let a_ip = [10, 0, 0, 1];
        let b_ip = [10, 0, 0, 2];
        let syn = SynthPacket {
            eth_src: a_mac,
            eth_dst: b_mac,
            ethertype: 0x0800,
            ipv4: Some(SynthIpv4 {
                src_ip: a_ip,
                dst_ip: b_ip,
                ip_id: 0,
                proto: 6,
                tcp: Some(SynthTcp {
                    src_port: 40000,
                    dst_port: 10001,
                    seq: a_iss,
                    ack: 0,
                    data_offset: 20,
                    flags: TCP_FLAG_SYN,
                    window: 64240,
                    options: Vec::new(),
                    payload: payload.to_vec(),
                }),
            }),
            raw_payload: Vec::new(),
        };
        build_pcap(&[syn])
    };
    let pcap_a = flow(0xDEAD_BEEF, b"hello");
    let pcap_b = flow(0x0000_0007, b"world"); // different payload → divergence
    let opts = CanonicalizationOptions::default();
    let can_a = canonicalize_pcap(&pcap_a, &opts).expect("canonicalise A");
    let can_b = canonicalize_pcap(&pcap_b, &opts).expect("canonicalise B");
    assert!(
        byte_diff_count(&can_a, &can_b) > 0,
        "payload divergence must survive canonicalisation"
    );
}

#[test]
fn canonicalize_is_idempotent() {
    // Canonicalising an already-canonicalised pcap is a no-op.
    let pcap = build_handshake_with_tsopt(mac(0x01), mac(0x02), 0xDEAD_BEEF, 0xFACE_F00D, 100, 200, true);
    let opts = CanonicalizationOptions::default();
    let once = canonicalize_pcap(&pcap, &opts).expect("first pass");
    let twice = canonicalize_pcap(&once, &opts).expect("second pass");
    assert_eq!(once, twice, "canonicalise must be idempotent");
}

#[test]
fn canonicalize_rejects_garbage_pcap_cleanly() {
    // Not a pcap — should return Err, not panic.
    let garbage = vec![0u8; 8];
    let opts = CanonicalizationOptions::default();
    let err = canonicalize_pcap(&garbage, &opts);
    assert!(err.is_err(), "garbage input must error cleanly");
}

#[test]
fn canonicalize_matches_on_empty_pcap() {
    // Empty pcap (header only, no packets). Cursor reads will return
    // None immediately; canonicalisation should produce the same header.
    let mut buf = Vec::new();
    {
        let _ = PcapWriter::new(&mut buf).expect("PcapWriter::new");
    }
    let opts = CanonicalizationOptions::default();
    let canon = canonicalize_pcap(&buf, &opts).expect("empty pcap must roundtrip");
    // The canonicalised pcap may differ from the input if the writer
    // picks up different defaults — compare via parser instead.
    let mut r = pcap_file::pcap::PcapReader::new(Cursor::new(canon)).expect("reader");
    assert!(r.next_packet().is_none());
}
