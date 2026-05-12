//! bench-micro::rx_prelude — per-RX-frame L2 + L3 decode prelude cost.
//!
//! The RX prelude is the chain of pure-function decoders that runs on
//! every received frame in production BEFORE `tcp_input::dispatch`. The
//! production call-chain lives in `engine.rs` at:
//!   * `Engine::rx_frame` (engine.rs:4313) — entry from the per-frame
//!     dispatcher; calls `l2::l2_decode`.
//!   * `Engine::handle_ipv4` (engine.rs:4388) — IPv4 branch; calls
//!     `l3_ip::ip_decode_offload_aware` which in turn calls
//!     `classify_ip_rx_cksum` then `ip_decode` (with `nic_csum_ok=true`
//!     on GOOD, `false` on NONE/UNKNOWN, drops on BAD).
//!   * `Engine::tcp_input` prelude (engine.rs:4496-4553) — runs
//!     `classify_l4_rx_cksum` before handing off to
//!     `tcp_input::parse_segment`.
//!
//! The existing `tcp_input` benches (bench-micro::tcp_input) start from
//! a pre-built `ParsedSegment` — they skip the entire L2/L3 prelude.
//! This bench isolates those prelude steps.
//!
//! # Variants
//!
//! * `bench_l2_decode` — `l2::l2_decode` on a 14-byte Ethernet header
//!   leading a valid IPv4 frame; `our_mac` matches the dst MAC so the
//!   function returns `Ok(L2Decoded::ETHERTYPE_IPV4)`. Sub-15 ns; uses
//!   `iter_custom` + BATCH=128 to amortize criterion's per-iter
//!   overhead.
//! * `bench_internet_checksum_ipv4_hdr` — `l3_ip::internet_checksum`
//!   folded over a 20-byte IPv4 header (no options). Pure-compute,
//!   sub-10 ns; uses `iter_custom` + BATCH=128. This is what the
//!   `nic_csum_ok=false` path inside `ip_decode` runs once per frame
//!   (l3_ip.rs:109) on a NIC that didn't validate.
//! * `bench_internet_checksum_tcp_segment` — same fold over a typical
//!   100-byte buffer (TCP header + small payload), showing the per-byte
//!   scaling of the fold. Production `ip_decode` only folds the IP
//!   header, but the same fold is what `tcp_output::tcp_pseudo_header_checksum`
//!   and the TCP-segment checksum verify in `parse_segment` use, so the
//!   per-byte cost is shared.
//! * `bench_ip_decode_swcksum` — `l3_ip::ip_decode` with
//!   `nic_csum_ok=false`. Forces the SW checksum-fold path
//!   (l3_ip.rs:104-114) — the steady-state cost when the NIC didn't or
//!   couldn't validate the IPv4 header.
//! * `bench_ip_decode_offload_ok` — `l3_ip::ip_decode` with
//!   `nic_csum_ok=true`. Skips the SW checksum fold; this is what
//!   `ip_decode_offload_aware` calls on `CksumOutcome::Good`. The cost
//!   delta vs. `_swcksum` isolates the SW-fold overhead specifically.
//! * `bench_classify_rx_cksum` — `classify_ip_rx_cksum` paired with
//!   `classify_l4_rx_cksum`, which is what `ip_decode_offload_aware` +
//!   `Engine::tcp_input` prelude do per-frame in production
//!   (engine.rs:4527). Sub-10 ns combined; `iter_custom` + BATCH=128.
//!
//! # Scope caveats
//!
//! Per-frame prelude ONLY. This bench does NOT cover:
//!   * RSS-hash classification or any RSS-aware path (`nic_rss_hash`
//!     thread-through is engine-only).
//!   * The ARP / ICMP demux branches inside `handle_ipv4` /
//!     `handle_arp` — only the IPv4-TCP happy path is measured.
//!   * mbuf metadata reads (`mbuf.data_off`, `data_len`, `ol_flags`,
//!     `hash.rss`, `dynfield1`) — these are real DPDK shim calls in
//!     production but the bench passes plain `&[u8]` slices and `u64`
//!     literals.
//!   * `tcp_input::parse_segment` itself (covered by the existing
//!     bench-micro::tcp_input benches, which start from a built
//!     `ParsedSegment`).
//!   * The drop-path counter bumps and the per-drop-reason match arms
//!     in `Engine::rx_frame` + `handle_ipv4` (engine.rs:4322-4486) —
//!     production calls `counters::inc` on every error branch; happy-path
//!     traffic never hits them.
//!
//! These exclusions are intentional: this bench measures the
//! pure-function decoder cost. Engine-side surrounding work (counter
//! bumps on the happy path, RSS demux, ARP, mbuf metadata) is bundled
//! into the higher-level T1 `bench_send_*` and the bench-rx-burst
//! integration benches.
//!
//! # `black_box` discipline
//!
//! For `l2_decode` and `ip_decode` the `Result<..., Drop>` is unwrapped
//! into a fixed accumulator (folding `payload_offset` + `ethertype` for
//! L2, and `header_len` + `total_len` + `protocol` + `src_ip` + `dst_ip`
//! for L3) so LLVM cannot DCE the decoded fields. The input slice is
//! `black_box`'d on every call to prevent constant-folding the byte
//! layout. The accumulator is `black_box`'d once per BATCH.
//!
//! For `internet_checksum` the returned `u16` is XOR-folded across the
//! batch and `black_box`'d once per batch; the input chunks are
//! `black_box`'d every call.
//!
//! For `classify_*_rx_cksum` the `CksumOutcome` enums map onto a u8
//! discriminant via `as u8` and are XOR-folded.

use criterion::{black_box, criterion_group, criterion_main, Criterion};
use dpdk_net_core::l2::l2_decode;
use dpdk_net_core::l3_ip::{
    classify_ip_rx_cksum, classify_l4_rx_cksum, internet_checksum, ip_decode, CksumOutcome,
};
use std::time::{Duration, Instant};

/// Batching factor for `iter_custom`. Mirrors `bench_tsc_read_*` /
/// `bench_parse_options` / `bench_pseudo_header_checksum` BATCH=128 —
/// the prelude decoders are all sub-50 ns where criterion's per-iter
/// closure-call + sample-bookkeeping overhead can otherwise dominate.
const BATCH: u64 = 128;

/// Production-realistic MAC + IP wiring, matching the
/// `bench-micro::build_segment` constants so the cross-bench numerical
/// story is internally consistent.
const OUR_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x01];
const PEER_MAC: [u8; 6] = [0x02, 0x00, 0x00, 0x00, 0x00, 0x02];
const OUR_IP: u32 = 0x0a_00_00_02; // 10.0.0.2 — bench DUT
const PEER_IP: u32 = 0x0a_00_00_01; // 10.0.0.1 — bench peer
const OUR_PORT: u16 = 40000;
const PEER_PORT: u16 = 5000;

/// IPv4 TCP protocol number (l3_ip::IPPROTO_TCP).
const IPPROTO_TCP: u8 = 6;
/// Ethertype IPv4 (l2::ETHERTYPE_IPV4).
const ETHERTYPE_IPV4: u16 = 0x0800;

/// Build a full Ethernet + IPv4 + TCP frame with the given payload,
/// computing a correct IPv4 header checksum so `ip_decode` with
/// `nic_csum_ok=false` accepts it. The TCP-segment checksum field is
/// left zero — this bench does NOT exercise `parse_segment` or any
/// L4 software verify, so the zero TCP checksum is intentional.
///
/// Layout:
///   * 14 B Ethernet (dst, src, ethertype 0x0800)
///   * 20 B IPv4 (version 4 IHL 5, no options, TTL 64, proto TCP, DF)
///   * 20 B TCP (header only, no options, ACK flag set)
///   * `payload.len()` bytes payload
fn build_frame(payload: &[u8]) -> Vec<u8> {
    let total_ip_len = 20 + 20 + payload.len();
    assert!(
        total_ip_len <= u16::MAX as usize,
        "IPv4 total_length overflow"
    );
    let mut buf = Vec::with_capacity(14 + total_ip_len);

    // --- Ethernet (14 B) ---
    buf.extend_from_slice(&OUR_MAC); // dst (we are receiving)
    buf.extend_from_slice(&PEER_MAC); // src
    buf.extend_from_slice(&ETHERTYPE_IPV4.to_be_bytes());

    // --- IPv4 (20 B, no options) ---
    let ip_start = buf.len();
    buf.push(0x45); // version 4, IHL 5
    buf.push(0x00); // DSCP/ECN
    buf.extend_from_slice(&(total_ip_len as u16).to_be_bytes()); // total length
    buf.extend_from_slice(&0x0001u16.to_be_bytes()); // identification
    buf.extend_from_slice(&0x4000u16.to_be_bytes()); // flags=DF, offset 0
    buf.push(0x40); // TTL 64
    buf.push(IPPROTO_TCP); // protocol = TCP
    buf.extend_from_slice(&[0x00, 0x00]); // checksum placeholder
    buf.extend_from_slice(&PEER_IP.to_be_bytes()); // src IP
    buf.extend_from_slice(&OUR_IP.to_be_bytes()); // dst IP

    // Compute and patch IPv4 header checksum. Using internet_checksum
    // on the 20-byte header with the checksum field zeroed gives the
    // correct value to insert (RFC 1071 + RFC 791 §3.1).
    let ip_csum = internet_checksum(&[&buf[ip_start..ip_start + 20]]);
    buf[ip_start + 10] = (ip_csum >> 8) as u8;
    buf[ip_start + 11] = (ip_csum & 0xff) as u8;

    // --- TCP (20 B, no options) ---
    buf.extend_from_slice(&PEER_PORT.to_be_bytes()); // src port
    buf.extend_from_slice(&OUR_PORT.to_be_bytes()); // dst port
    buf.extend_from_slice(&0x0000_1001u32.to_be_bytes()); // seq
    buf.extend_from_slice(&0x0000_5001u32.to_be_bytes()); // ack
    buf.push(0x50); // data offset = 5 (20 B header), reserved 0
    buf.push(0x10); // flags: ACK
    buf.extend_from_slice(&1024u16.to_be_bytes()); // window
    buf.extend_from_slice(&[0x00, 0x00]); // checksum (zero — not verified by this bench)
    buf.extend_from_slice(&[0x00, 0x00]); // urgent pointer

    // --- Payload ---
    buf.extend_from_slice(payload);

    debug_assert_eq!(buf.len(), 14 + total_ip_len);
    buf
}

/// `bench_l2_decode` — Ethernet-header decode for a valid IPv4 frame
/// addressed to our MAC. Returns `Ok(L2Decoded { ethertype=0x0800,
/// payload_offset=14, .. })`. The `our_mac` parameter matches the dst
/// MAC inside the frame so neither the `MissMac` nor the broadcast
/// path is taken — measures the steady-state happy path.
///
/// BATCH=128 + `iter_custom` because `l2_decode` is on the order of
/// 5-15 ns; criterion's per-iter overhead would dominate without
/// batching.
fn bench_l2_decode(c: &mut Criterion) {
    c.bench_function("bench_l2_decode", |b| {
        let frame = build_frame(&[]);
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                let mut acc: u64 = 0;
                for _ in 0..BATCH {
                    let r = l2_decode(black_box(&frame), OUR_MAC);
                    match r {
                        Ok(d) => {
                            // Fold every field of the decoded struct so
                            // LLVM cannot prove the unused-field decode
                            // is dead.
                            acc ^= d.ethertype as u64;
                            acc ^= (d.payload_offset as u64) << 16;
                            for &b in &d.src_mac {
                                acc ^= b as u64;
                            }
                            for &b in &d.dst_mac {
                                acc ^= b as u64;
                            }
                        }
                        Err(_) => panic!("bench setup: l2_decode rejected a valid frame"),
                    }
                }
                black_box(acc);
            }
            start.elapsed() / (BATCH as u32)
        });
    });
}

/// `bench_internet_checksum_ipv4_hdr` — RFC 1071 fold over a 20-byte
/// IPv4 header (no options). This is the exact input shape the SW
/// `ip_decode` checksum verify uses at l3_ip.rs:109. Pure compute,
/// expected sub-10 ns; BATCH=128 + `iter_custom` amortizes criterion
/// overhead.
fn bench_internet_checksum_ipv4_hdr(c: &mut Criterion) {
    c.bench_function("bench_internet_checksum_ipv4_hdr", |b| {
        // Extract the 20-byte IPv4 header from a built frame so the
        // input shape exactly matches what `ip_decode`'s scratch path
        // sees in production.
        let frame = build_frame(&[]);
        let ip_hdr = frame[14..34].to_vec();
        assert_eq!(ip_hdr.len(), 20);
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                let mut acc: u64 = 0;
                for _ in 0..BATCH {
                    // black_box on the slice reference prevents
                    // constant-folding the fold against the
                    // setup-time-known header bytes.
                    let cks = internet_checksum(&[black_box(ip_hdr.as_slice())]);
                    acc ^= cks as u64;
                }
                black_box(acc);
            }
            start.elapsed() / (BATCH as u32)
        });
    });
}

/// `bench_internet_checksum_tcp_segment` — RFC 1071 fold over a typical
/// ~100 B TCP segment (20 B TCP header + ~80 B payload). Production
/// `ip_decode`'s SW path only folds the 20 B IPv4 header, but the same
/// `internet_checksum` function is used inside `parse_segment` for the
/// TCP-segment checksum verify and by `tcp_pseudo_header_checksum`.
/// This bench measures the per-byte scaling: cost ≈ ipv4-hdr-cost +
/// (segment_len - 20) / 2 cycles, approximately.
fn bench_internet_checksum_tcp_segment(c: &mut Criterion) {
    c.bench_function("bench_internet_checksum_tcp_segment", |b| {
        // 100 B total: 20 B TCP header + 80 B payload. Non-zero
        // payload bytes so the fold has actual work (a zero-byte
        // payload could let the optimizer shortcut). Bytes derived
        // from a sliced built frame's TCP region for shape parity.
        let frame = build_frame(&[0x42u8; 80]);
        let tcp_seg = frame[34..134].to_vec();
        assert_eq!(tcp_seg.len(), 100);
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                let mut acc: u64 = 0;
                for _ in 0..BATCH {
                    let cks = internet_checksum(&[black_box(tcp_seg.as_slice())]);
                    acc ^= cks as u64;
                }
                black_box(acc);
            }
            start.elapsed() / (BATCH as u32)
        });
    });
}

/// `bench_ip_decode_swcksum` — `ip_decode` with `nic_csum_ok=false`.
/// Forces the SW IPv4-header checksum fold at l3_ip.rs:104-114. This
/// is the steady-state cost on a NIC that did not validate the IP
/// header (either because `hw-offload-rx-cksum` is feature-off, the
/// runtime `rx_cksum_offload_active` latch is false, or the PMD
/// reported NONE/UNKNOWN). Expected 20-40 ns; uses `iter` (non-batched)
/// since the workload is well above criterion's per-iter overhead.
fn bench_ip_decode_swcksum(c: &mut Criterion) {
    c.bench_function("bench_ip_decode_swcksum", |b| {
        let frame = build_frame(&[]);
        // ip_decode takes the IPv4 packet starting at the IP header,
        // not the full Ethernet frame. Strip the 14-byte Eth header.
        let ip_pkt = frame[14..].to_vec();
        b.iter(|| {
            let r = ip_decode(black_box(&ip_pkt), OUR_IP, false);
            match r {
                Ok(d) => {
                    // Observe every decoded field so LLVM keeps the
                    // header-parse stores alive.
                    black_box(d.protocol);
                    black_box(d.src_ip);
                    black_box(d.dst_ip);
                    black_box(d.header_len);
                    black_box(d.total_len);
                    black_box(d.ttl);
                }
                Err(_) => panic!("bench setup: ip_decode rejected a valid header"),
            }
        });
    });
}

/// `bench_ip_decode_offload_ok` — `ip_decode` with `nic_csum_ok=true`.
/// Skips the SW checksum fold; this is what `ip_decode_offload_aware`
/// calls on `CksumOutcome::Good`. The delta vs. `_swcksum` isolates
/// the SW-fold cost from the rest of the IPv4 decode (version, IHL,
/// total_len, fragment-flag, TTL, src/dst extraction). Expected
/// 10-20 ns.
fn bench_ip_decode_offload_ok(c: &mut Criterion) {
    c.bench_function("bench_ip_decode_offload_ok", |b| {
        let frame = build_frame(&[]);
        let ip_pkt = frame[14..].to_vec();
        b.iter(|| {
            let r = ip_decode(black_box(&ip_pkt), OUR_IP, true);
            match r {
                Ok(d) => {
                    black_box(d.protocol);
                    black_box(d.src_ip);
                    black_box(d.dst_ip);
                    black_box(d.header_len);
                    black_box(d.total_len);
                    black_box(d.ttl);
                }
                Err(_) => panic!("bench setup: ip_decode rejected a valid header"),
            }
        });
    });
}

/// `bench_classify_rx_cksum` — `classify_ip_rx_cksum` paired with
/// `classify_l4_rx_cksum` on `RTE_MBUF_F_RX_IP_CKSUM_GOOD |
/// RTE_MBUF_F_RX_L4_CKSUM_GOOD` flags. Production runs both per-frame:
/// the IP classifier inside `ip_decode_offload_aware`
/// (l3_ip.rs:211) and the L4 classifier in `Engine::tcp_input`
/// (engine.rs:4527). Sub-10 ns combined; uses `iter_custom` + BATCH=128.
///
/// Note: both classifiers are feature-gated on `hw-offload-rx-cksum`,
/// which is on by default in the workspace. A feature-off build would
/// not compile this bench. The bench-micro Cargo.toml's `dpdk-net-core`
/// dep uses default features, so the gate is satisfied.
fn bench_classify_rx_cksum(c: &mut Criterion) {
    // Hand-mirrored from `dpdk_net_core::dpdk_consts`:
    //   RTE_MBUF_F_RX_IP_CKSUM_GOOD = 1<<7  (= 0x80)
    //   RTE_MBUF_F_RX_L4_CKSUM_GOOD = 1<<8  (= 0x100)
    // The combined "PMD reported GOOD for both IP + L4" pattern is
    // what the steady-state ENA RX path delivers.
    const OL_FLAGS_BOTH_GOOD: u64 = (1u64 << 7) | (1u64 << 8);

    c.bench_function("bench_classify_rx_cksum", |b| {
        b.iter_custom(|iters| {
            let start = Instant::now();
            for _ in 0..iters {
                let mut acc: u64 = 0;
                for _ in 0..BATCH {
                    let ip = classify_ip_rx_cksum(black_box(OL_FLAGS_BOTH_GOOD));
                    let l4 = classify_l4_rx_cksum(black_box(OL_FLAGS_BOTH_GOOD));
                    // Map the CksumOutcome enums onto u8 discriminants
                    // so the values are observable to LLVM. We assert
                    // the expected variant in a sentinel debug check
                    // below to catch bench-setup drift if the flag
                    // mapping ever changes.
                    acc ^= match ip {
                        CksumOutcome::Good => 1,
                        CksumOutcome::Bad => 2,
                        CksumOutcome::None => 3,
                        CksumOutcome::Unknown => 4,
                    };
                    acc ^= match l4 {
                        CksumOutcome::Good => 1 << 8,
                        CksumOutcome::Bad => 2 << 8,
                        CksumOutcome::None => 3 << 8,
                        CksumOutcome::Unknown => 4 << 8,
                    };
                }
                black_box(acc);
            }
            start.elapsed() / (BATCH as u32)
        });
    });
}

criterion_group! {
    name = benches;
    config = Criterion::default()
        .warm_up_time(Duration::from_secs(1))
        .measurement_time(Duration::from_secs(5))
        .sample_size(100);
    targets =
        bench_l2_decode,
        bench_internet_checksum_ipv4_hdr,
        bench_internet_checksum_tcp_segment,
        bench_ip_decode_swcksum,
        bench_ip_decode_offload_ok,
        bench_classify_rx_cksum,
}
criterion_main!(benches);
