//! A10 Plan B Task 9 — pcap divergence-normalisation for mode B.
//!
//! Mode B (spec §8) verifies that dpdk_net with `preset=rfc_compliance`
//! emits byte-identical wire traffic to Linux TCP with defaults, given
//! the same workload. Two free values diverge by design and must be
//! rewritten before a byte-diff can succeed:
//!
//! 1. **Initial sequence number (ISS).** RFC 6528 host-chosen; both
//!    stacks pick their own random ISS per direction. We pin each
//!    direction to a canonical ISS so downstream seq / ack numbers
//!    fall into a known range.
//! 2. **TCP Timestamp base (TSval).** RFC 7323 makes TSval a free-
//!    running clock starting at any host-chosen offset. Both stacks
//!    pick independently. We pin each direction's observed base
//!    TSval (the first TSval seen on that direction) to a canonical
//!    value; TSecr mirrors the peer's base.
//! 3. **MAC addresses.** Capture happens on different hosts so the
//!    source MAC of the DUT side vs. the Linux side is physically
//!    different. Normalise both to canonical bytes, direction-aware.
//!
//! Anything else that diverges after this normalisation is an actual
//! wire-behaviour divergence and the byte-diff will flag it.
//!
//! # Scope — packets we canonicalise
//!
//! Only Ethernet / IPv4 / TCP packets with a well-formed header chain
//! are rewritten. Non-IPv4 frames, non-TCP IPv4 datagrams, and packets
//! too short to parse pass through unchanged (their bytes are still
//! included in the diff; they just aren't touched).
//!
//! # Scope — what we do NOT touch
//!
//! - IPv4 Identification field (`ip_id`): not rewritten. Linux uses a
//!   per-destination PRNG; A10 hasn't decided on dpdk_net's ID policy.
//!   Divergence here is expected; operators should explicitly decide
//!   whether to include ip_id in the canonical set (follow-up to T9).
//! - TCP window (`window`): not rewritten. A wire-diff mismatch here
//!   is meaningful (window-scaling divergence) so we preserve it.
//! - TCP MSS option (kind=2): not rewritten. MSS depends on MTU; spec
//!   §8 explicitly documents it as an expected divergence until the
//!   operator pins both stacks to a matching MTU.
//! - Urgent pointer: pass-through.
//! - IP DSCP/ECN: pass-through.
//! - IP flags / fragmentation offset: pass-through (fragmented TCP is
//!   out of scope for the A10 workload).
//! - Pcap timestamps: the pcap record header's `ts_sec` / `ts_frac`
//!   are preserved verbatim. They're wall-clock capture timestamps,
//!   not wire bytes, and the diff operates on wire bytes only.
//!
//! # Direction identification
//!
//! A TCP connection has two directions. We identify each direction by
//! the (src_ip, src_port, dst_ip, dst_port) 4-tuple and build a pair
//! of state slots: one for the lex-smaller side, one for the larger.
//! This lets us consistently assign canonical MACs and pin canonical
//! ISS / TS bases without knowing in advance which host (DUT vs. peer)
//! captured which direction.
//!
//! # Checksum recompute
//!
//! After any rewrite that affects the IPv4 or TCP header, we recompute
//! the IPv4 header checksum and the TCP checksum (which covers the
//! pseudo-header). This means normalised captures are *always* valid
//! wire-format traffic; they're not just memory-diffable blobs.

use std::collections::HashMap;
use std::io::Cursor;

use pcap_file::pcap::{PcapHeader, PcapPacket, PcapReader, PcapWriter};

/// Options controlling canonicalisation. All fields have trading-
/// latency-sensible defaults matching the Task 9 plan.
#[derive(Debug, Clone)]
pub struct CanonicalizationOptions {
    /// Fixed ISS replacement for each direction. `(observed_seq -
    /// observed_iss) + canonical_iss` becomes the new seq number.
    pub canonical_iss: u32,
    /// Fixed TSval base replacement. `(observed_tsval -
    /// observed_tsval_base) + canonical_ts_base` becomes the new
    /// TSval. TSecr tracks the peer's base the same way.
    pub canonical_ts_base: u32,
    /// Canonical MAC for the direction whose source IP + port sort
    /// lex-smaller. The reverse direction gets `canonical_dst_mac`.
    pub canonical_src_mac: [u8; 6],
    /// Canonical MAC for the direction whose source IP + port sort
    /// lex-larger.
    pub canonical_dst_mac: [u8; 6],
}

impl Default for CanonicalizationOptions {
    fn default() -> Self {
        Self {
            canonical_iss: 0x1234_5678,
            canonical_ts_base: 0xAABB_CCDD,
            canonical_src_mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
            canonical_dst_mac: [0x02, 0x00, 0x00, 0x00, 0x00, 0x02],
        }
    }
}

/// Canonicalisation errors. Internal to the normalize module; surface
/// to callers as `anyhow::Error` via `?` in the runner.
#[derive(Debug, thiserror::Error)]
pub enum CanonError {
    #[error("pcap parse error: {0}")]
    Pcap(#[from] pcap_file::PcapError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Rewrite every IPv4 / TCP packet in `pcap_bytes` per `opts`,
/// returning the canonicalised pcap as a `Vec<u8>`. Non-IPv4 or
/// non-TCP packets pass through unchanged.
///
/// The implementation is two-pass: the first pass scans every packet
/// to discover per-direction ISS and TSval-base pins (the first
/// packet seen on that direction), the second pass rewrites using the
/// fully-populated pin table. Two passes are required for mid-stream
/// captures where a direction's first observed packet is an ACK
/// whose ack-number belongs to a direction we haven't seen yet:
/// without pre-scan we'd leave that ACK number unchanged and produce
/// a spurious divergence on byte-diff. Memory cost is bounded by the
/// number of directions in the capture, which is at most 2×(flow
/// count) — small and not a concern for the A10 workload scale.
pub fn canonicalize_pcap(
    pcap_bytes: &[u8],
    opts: &CanonicalizationOptions,
) -> Result<Vec<u8>, CanonError> {
    // Pass 1: discover pins.
    let mut state = FlowState::default();
    {
        let cursor = Cursor::new(pcap_bytes);
        let mut reader = PcapReader::new(cursor)?;
        while let Some(pkt) = reader.next_packet() {
            let pkt = pkt?;
            discover_pins(&pkt.data, &mut state);
        }
    }

    // Pass 2: rewrite using the populated pin table.
    let cursor = Cursor::new(pcap_bytes);
    let mut reader = PcapReader::new(cursor)?;
    let in_header: PcapHeader = reader.header();

    let mut out_buf: Vec<u8> = Vec::with_capacity(pcap_bytes.len());
    {
        let mut writer = PcapWriter::with_header(&mut out_buf, in_header)?;
        while let Some(pkt) = reader.next_packet() {
            let pkt = pkt?;
            let ts = pkt.timestamp;
            let orig_len = pkt.orig_len;
            let mut data = pkt.data.into_owned();
            rewrite_frame(&mut data, opts, &state);
            let out_pkt = PcapPacket::new_owned(ts, orig_len, data);
            writer.write_packet(&out_pkt)?;
        }
    }
    Ok(out_buf)
}

/// Pass 1 walker: note the ISS (first seq seen per direction) and TS
/// base (first TSval seen per direction) without rewriting. Shares
/// the frame walker with `rewrite_frame`; factored as a separate
/// function so the rewrite path can take `&FlowState` instead of
/// `&mut` and stay read-only.
fn discover_pins(data: &[u8], state: &mut FlowState) {
    // Same parse guards as rewrite_frame. Any failed guard drops the
    // packet from the pin-discovery walk (consistent with the rewrite
    // path skipping it too).
    if data.len() < 14 {
        return;
    }
    let ethertype = u16::from_be_bytes([data[12], data[13]]);
    if ethertype != ETHERTYPE_IPV4 {
        return;
    }
    if data.len() < 14 + 20 {
        return;
    }
    let ihl = (data[14] & 0x0F) as usize * 4;
    if ihl < 20 || data.len() < 14 + ihl {
        return;
    }
    let proto = data[14 + 9];
    if proto != IPPROTO_TCP {
        return;
    }
    let src_ip = u32::from_be_bytes([data[14 + 12], data[14 + 13], data[14 + 14], data[14 + 15]]);
    let dst_ip = u32::from_be_bytes([data[14 + 16], data[14 + 17], data[14 + 18], data[14 + 19]]);
    let ip_total_len = u16::from_be_bytes([data[14 + 2], data[14 + 3]]) as usize;
    if 14 + ip_total_len > data.len() {
        return;
    }
    let tcp_off = 14 + ihl;
    if tcp_off + 20 > data.len() {
        return;
    }
    let src_port = u16::from_be_bytes([data[tcp_off], data[tcp_off + 1]]);
    let dst_port = u16::from_be_bytes([data[tcp_off + 2], data[tcp_off + 3]]);
    let tcp_data_off = ((data[tcp_off + 12] >> 4) & 0x0F) as usize * 4;
    if tcp_data_off < 20 || tcp_off + tcp_data_off > 14 + ip_total_len {
        return;
    }
    let seq = u32::from_be_bytes([
        data[tcp_off + 4],
        data[tcp_off + 5],
        data[tcp_off + 6],
        data[tcp_off + 7],
    ]);
    let (dir_key, is_low) = classify_direction(src_ip, src_port, dst_ip, dst_port);
    // First seq observed on this direction pins the ISS. We don't
    // privilege SYN here — a mid-stream capture has no SYN for at
    // least one side, and anchoring to the first-seen seq gives a
    // deterministic pin for both inputs.
    state.iss.entry((dir_key, is_low)).or_insert(seq);

    // Walk options for TSval — only needed to pin the base.
    if tcp_data_off > 20 {
        let opts_slice = &data[tcp_off + 20..tcp_off + tcp_data_off];
        let mut i = 0;
        while i < opts_slice.len() {
            let kind = opts_slice[i];
            if kind == 0 {
                break;
            }
            if kind == 1 {
                i += 1;
                continue;
            }
            if i + 1 >= opts_slice.len() {
                break;
            }
            let len = opts_slice[i + 1] as usize;
            if len < 2 || i + len > opts_slice.len() {
                break;
            }
            if kind == 8 && len == 10 {
                let tsval = u32::from_be_bytes([
                    opts_slice[i + 2],
                    opts_slice[i + 3],
                    opts_slice[i + 4],
                    opts_slice[i + 5],
                ]);
                state.ts_base.entry((dir_key, is_low)).or_insert(tsval);
            }
            i += len;
        }
    }
}

// ---------------------------------------------------------------------------
// Per-flow state — observed ISS + TSval base per direction.
// ---------------------------------------------------------------------------

#[derive(Default)]
struct FlowState {
    /// Per-direction observed-ISS pins. Direction is the sorted-tuple
    /// key — see `direction_key`. `(key, is_low)` maps to the ISS for
    /// the side that sent the first SYN in that direction.
    iss: HashMap<(DirectionKey, bool), u32>,
    /// Per-direction observed-TSval base.
    ts_base: HashMap<(DirectionKey, bool), u32>,
}

/// Connection identifier independent of direction. Built by sorting
/// the two `(ip, port)` endpoints lex-smallest-first. A flag passed
/// alongside this key indicates which side of the sorted pair the
/// packet originates from.
#[derive(Hash, PartialEq, Eq, Clone, Copy)]
struct DirectionKey {
    /// Lex-smaller (src_ip, src_port) pair, in network-byte-order.
    low_ip: u32,
    low_port: u16,
    /// Lex-larger (dst_ip, dst_port) pair, in network-byte-order.
    high_ip: u32,
    high_port: u16,
}

/// Build the direction key + `is_low` flag. `is_low = true` means the
/// packet was sent by the lex-smaller endpoint (source matches `low`).
fn classify_direction(
    src_ip: u32,
    src_port: u16,
    dst_ip: u32,
    dst_port: u16,
) -> (DirectionKey, bool) {
    let s = (src_ip, src_port);
    let d = (dst_ip, dst_port);
    if s <= d {
        (
            DirectionKey {
                low_ip: s.0,
                low_port: s.1,
                high_ip: d.0,
                high_port: d.1,
            },
            true,
        )
    } else {
        (
            DirectionKey {
                low_ip: d.0,
                low_port: d.1,
                high_ip: s.0,
                high_port: s.1,
            },
            false,
        )
    }
}

// ---------------------------------------------------------------------------
// Frame rewriter.
// ---------------------------------------------------------------------------

const ETHERTYPE_IPV4: u16 = 0x0800;
const IPPROTO_TCP: u8 = 6;
const TCP_FLAG_ACK: u8 = 0x10;

fn rewrite_frame(data: &mut [u8], opts: &CanonicalizationOptions, state: &FlowState) {
    // 14-byte Ethernet header; anything shorter is a truncated capture
    // and we pass through (canonicalisation is lossless for unparseable
    // frames).
    if data.len() < 14 {
        return;
    }
    let ethertype = u16::from_be_bytes([data[12], data[13]]);
    if ethertype != ETHERTYPE_IPV4 {
        return;
    }

    // IPv4 header starts at offset 14.
    if data.len() < 14 + 20 {
        return;
    }
    let ihl = (data[14] & 0x0F) as usize * 4;
    if ihl < 20 || data.len() < 14 + ihl {
        return;
    }
    let proto = data[14 + 9];
    if proto != IPPROTO_TCP {
        return;
    }
    let src_ip = u32::from_be_bytes([data[14 + 12], data[14 + 13], data[14 + 14], data[14 + 15]]);
    let dst_ip = u32::from_be_bytes([data[14 + 16], data[14 + 17], data[14 + 18], data[14 + 19]]);
    let ip_total_len = u16::from_be_bytes([data[14 + 2], data[14 + 3]]) as usize;
    if 14 + ip_total_len > data.len() {
        // Truncated L3 payload; leave alone.
        return;
    }

    // TCP header at offset 14 + ihl.
    let tcp_off = 14 + ihl;
    if tcp_off + 20 > data.len() {
        return;
    }
    let src_port = u16::from_be_bytes([data[tcp_off], data[tcp_off + 1]]);
    let dst_port = u16::from_be_bytes([data[tcp_off + 2], data[tcp_off + 3]]);
    let tcp_data_off = ((data[tcp_off + 12] >> 4) & 0x0F) as usize * 4;
    if tcp_data_off < 20 || tcp_off + tcp_data_off > 14 + ip_total_len {
        return;
    }
    let flags = data[tcp_off + 13];
    let seq =
        u32::from_be_bytes([data[tcp_off + 4], data[tcp_off + 5], data[tcp_off + 6], data[tcp_off + 7]]);
    let ack =
        u32::from_be_bytes([data[tcp_off + 8], data[tcp_off + 9], data[tcp_off + 10], data[tcp_off + 11]]);

    // Classify direction via the sorted-tuple key.
    let (dir_key, is_low) = classify_direction(src_ip, src_port, dst_ip, dst_port);
    let reverse_is_low = !is_low;

    // Pass 1 has already populated the ISS/TS-base pins for every
    // direction observed in the capture. Any direction without a pin
    // here means pass 1 saw no packet for it — which is only possible
    // if this packet itself was skipped during pass 1 (malformed
    // frame), and we already bailed above. So `iss` is guaranteed
    // present; the explicit `get` keeps the code defensive.
    let Some(&iss) = state.iss.get(&(dir_key, is_low)) else {
        return;
    };
    let reverse_iss = state.iss.get(&(dir_key, reverse_is_low));

    // Rewrite seq: (seq - iss) + canonical_iss, wrapping.
    let new_seq = opts.canonical_iss.wrapping_add(seq.wrapping_sub(iss));
    data[tcp_off + 4..tcp_off + 8].copy_from_slice(&new_seq.to_be_bytes());

    // Rewrite ACK if the reverse direction's ISS is known AND the ACK
    // flag is set. A SYN without ACK carries ack=0 by convention; we
    // leave that alone so the canonical stream's SYN also has ack=0
    // (not `canonical_iss`, which would be a divergence).
    if (flags & TCP_FLAG_ACK) != 0 {
        if let Some(&reverse_iss) = reverse_iss {
            let new_ack = opts.canonical_iss.wrapping_add(ack.wrapping_sub(reverse_iss));
            data[tcp_off + 8..tcp_off + 12].copy_from_slice(&new_ack.to_be_bytes());
        }
    }

    // Walk TCP options. Options live at [tcp_off + 20, tcp_off + tcp_data_off).
    rewrite_tcp_options(
        &mut data[tcp_off + 20..tcp_off + tcp_data_off],
        opts,
        state,
        dir_key,
        is_low,
        reverse_is_low,
    );

    // Rewrite MAC addresses direction-aware. Lex-smaller side always
    // uses canonical_src_mac as its L2 source; the other side uses
    // canonical_dst_mac. Destination MACs are swapped accordingly so
    // each packet's (src, dst) is a consistent pair.
    let (our_mac, their_mac) = if is_low {
        (opts.canonical_src_mac, opts.canonical_dst_mac)
    } else {
        (opts.canonical_dst_mac, opts.canonical_src_mac)
    };
    data[0..6].copy_from_slice(&their_mac); // dst
    data[6..12].copy_from_slice(&our_mac); // src

    // Recompute IPv4 header checksum.
    data[14 + 10] = 0;
    data[14 + 11] = 0;
    let ip_csum = internet_checksum(&data[14..14 + ihl]);
    data[14 + 10..14 + 12].copy_from_slice(&ip_csum.to_be_bytes());

    // Recompute TCP checksum over [pseudo-header, TCP header, TCP payload].
    let tcp_len = ip_total_len - ihl;
    let tcp_slice_end = tcp_off + tcp_len;
    // Zero the checksum field first.
    data[tcp_off + 16] = 0;
    data[tcp_off + 17] = 0;
    let tcp_csum = tcp_checksum(
        src_ip.to_be_bytes(),
        dst_ip.to_be_bytes(),
        tcp_len as u16,
        &data[tcp_off..tcp_slice_end],
    );
    data[tcp_off + 16..tcp_off + 18].copy_from_slice(&tcp_csum.to_be_bytes());
}

/// Rewrite TCP options in-place. Handles kind=8 (Timestamps) +
/// kind=5 (SACK) + kind=0 (EOL) / kind=1 (NOP) / kind=2 (MSS) /
/// kind=3 (Window Scale) / kind=4 (SACK Permitted). Unknown kinds
/// are skipped via their length byte.
fn rewrite_tcp_options(
    opts_slice: &mut [u8],
    canon: &CanonicalizationOptions,
    state: &FlowState,
    dir_key: DirectionKey,
    is_low: bool,
    reverse_is_low: bool,
) {
    let mut i = 0;
    while i < opts_slice.len() {
        let kind = opts_slice[i];
        match kind {
            0 => break, // EOL — stop.
            1 => {
                // NOP.
                i += 1;
            }
            _ => {
                // Kind + length pair. Length covers kind + length +
                // data. Truncated options (length==0 or past-end) are
                // treated as parse failure — bail rather than loop.
                if i + 1 >= opts_slice.len() {
                    break;
                }
                let len = opts_slice[i + 1] as usize;
                if len < 2 || i + len > opts_slice.len() {
                    break;
                }
                match kind {
                    8 if len == 10 => {
                        // Timestamps option: TSval(4) + TSecr(4).
                        let tsval = u32::from_be_bytes([
                            opts_slice[i + 2],
                            opts_slice[i + 3],
                            opts_slice[i + 4],
                            opts_slice[i + 5],
                        ]);
                        let tsecr = u32::from_be_bytes([
                            opts_slice[i + 6],
                            opts_slice[i + 7],
                            opts_slice[i + 8],
                            opts_slice[i + 9],
                        ]);
                        // Pass 1 has already recorded the per-direction
                        // TS base. Missing entry = no TS option ever
                        // seen on this direction pre-this-packet (only
                        // possible on malformed input); fall through
                        // to "this packet's tsval *is* the base" to
                        // keep output deterministic.
                        let base = *state.ts_base.get(&(dir_key, is_low)).unwrap_or(&tsval);
                        let new_tsval =
                            canon.canonical_ts_base.wrapping_add(tsval.wrapping_sub(base));
                        opts_slice[i + 2..i + 6].copy_from_slice(&new_tsval.to_be_bytes());
                        // TSecr echoes the peer's TSval. If we've pinned
                        // the peer's base, apply the same shift.
                        // Otherwise echo the raw value (zero on SYN per
                        // RFC 7323; non-zero only after peer speaks).
                        if tsecr != 0 {
                            if let Some(&reverse_base) =
                                state.ts_base.get(&(dir_key, reverse_is_low))
                            {
                                let new_tsecr = canon
                                    .canonical_ts_base
                                    .wrapping_add(tsecr.wrapping_sub(reverse_base));
                                opts_slice[i + 6..i + 10].copy_from_slice(&new_tsecr.to_be_bytes());
                            }
                        }
                    }
                    5 => {
                        // SACK option: blocks of (left_edge, right_edge)
                        // from the peer's seq space. Rewrite using the
                        // reverse direction's ISS pin.
                        if let Some(&reverse_iss) = state.iss.get(&(dir_key, reverse_is_low)) {
                            let blocks = (len - 2) / 8;
                            for b in 0..blocks {
                                let base_off = i + 2 + b * 8;
                                let left = u32::from_be_bytes([
                                    opts_slice[base_off],
                                    opts_slice[base_off + 1],
                                    opts_slice[base_off + 2],
                                    opts_slice[base_off + 3],
                                ]);
                                let right = u32::from_be_bytes([
                                    opts_slice[base_off + 4],
                                    opts_slice[base_off + 5],
                                    opts_slice[base_off + 6],
                                    opts_slice[base_off + 7],
                                ]);
                                let new_left =
                                    canon.canonical_iss.wrapping_add(left.wrapping_sub(reverse_iss));
                                let new_right = canon
                                    .canonical_iss
                                    .wrapping_add(right.wrapping_sub(reverse_iss));
                                opts_slice[base_off..base_off + 4]
                                    .copy_from_slice(&new_left.to_be_bytes());
                                opts_slice[base_off + 4..base_off + 8]
                                    .copy_from_slice(&new_right.to_be_bytes());
                            }
                        }
                    }
                    // kind=2 (MSS), kind=3 (Window Scale), kind=4 (SACK
                    // Permitted), and any unknown kind: pass through
                    // unchanged. MSS divergence is expected per spec §8
                    // and we explicitly do not rewrite it here.
                    _ => {}
                }
                i += len;
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Checksum helpers.
// ---------------------------------------------------------------------------

/// RFC 1071 internet checksum over an even-or-odd-length byte slice.
/// Returns the 16-bit complement ready to drop into the checksum field.
fn internet_checksum(bytes: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < bytes.len() {
        sum = sum.wrapping_add(u32::from(u16::from_be_bytes([bytes[i], bytes[i + 1]])));
        i += 2;
    }
    if i < bytes.len() {
        // Pad the trailing odd byte with zero.
        sum = sum.wrapping_add(u32::from(u16::from_be_bytes([bytes[i], 0])));
    }
    // Fold carries.
    while (sum >> 16) != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

/// TCP checksum: pseudo-header (src_ip, dst_ip, zero, proto, tcp_len)
/// + TCP header + payload.
fn tcp_checksum(src_ip: [u8; 4], dst_ip: [u8; 4], tcp_len: u16, tcp_bytes: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    // src_ip + dst_ip
    sum = sum
        .wrapping_add(u32::from(u16::from_be_bytes([src_ip[0], src_ip[1]])))
        .wrapping_add(u32::from(u16::from_be_bytes([src_ip[2], src_ip[3]])))
        .wrapping_add(u32::from(u16::from_be_bytes([dst_ip[0], dst_ip[1]])))
        .wrapping_add(u32::from(u16::from_be_bytes([dst_ip[2], dst_ip[3]])));
    // zero + proto
    sum = sum.wrapping_add(u32::from(IPPROTO_TCP));
    // tcp_len
    sum = sum.wrapping_add(u32::from(tcp_len));
    // TCP header + payload.
    let mut i = 0;
    while i + 1 < tcp_bytes.len() {
        sum = sum.wrapping_add(u32::from(u16::from_be_bytes([
            tcp_bytes[i],
            tcp_bytes[i + 1],
        ])));
        i += 2;
    }
    if i < tcp_bytes.len() {
        sum = sum.wrapping_add(u32::from(u16::from_be_bytes([tcp_bytes[i], 0])));
    }
    while (sum >> 16) != 0 {
        sum = (sum & 0xFFFF) + (sum >> 16);
    }
    !(sum as u16)
}

// ---------------------------------------------------------------------------
// Byte-diff helper — exposed as pub so mode_wire_diff can reuse it.
// ---------------------------------------------------------------------------

/// Compute the number of byte positions at which two slices differ.
/// Length mismatch counts every trailing byte as a divergence.
pub fn byte_diff_count(a: &[u8], b: &[u8]) -> usize {
    let common = a.len().min(b.len());
    let mut count = 0usize;
    for i in 0..common {
        if a[i] != b[i] {
            count += 1;
        }
    }
    count + a.len().abs_diff(b.len())
}

// ---------------------------------------------------------------------------
// Tests.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn internet_checksum_known_vector() {
        // RFC 1071 example: 0001 f203 f4f5 f6f7 → 220d
        let data = [0x00u8, 0x01, 0xf2, 0x03, 0xf4, 0xf5, 0xf6, 0xf7];
        assert_eq!(internet_checksum(&data), 0x220d);
    }

    #[test]
    fn internet_checksum_handles_odd_length() {
        let data = [0x12u8, 0x34, 0x56];
        // manually compute: 0x1234 + 0x5600 = 0x6834 → !0x6834 = 0x97CB
        assert_eq!(internet_checksum(&data), !0x6834);
    }

    #[test]
    fn byte_diff_count_identical_is_zero() {
        assert_eq!(byte_diff_count(&[1, 2, 3, 4], &[1, 2, 3, 4]), 0);
    }

    #[test]
    fn byte_diff_count_length_mismatch_counts_trailing() {
        assert_eq!(byte_diff_count(&[1, 2, 3], &[1, 2, 3, 4, 5]), 2);
    }

    #[test]
    fn byte_diff_count_counts_substitutions() {
        assert_eq!(byte_diff_count(&[1, 2, 3, 4], &[1, 9, 3, 9]), 2);
    }

    #[test]
    fn classify_direction_is_stable() {
        // swap src/dst: same key, opposite `is_low`.
        let (k1, l1) = classify_direction(0x0A00_0001, 1000, 0x0A00_0002, 2000);
        let (k2, l2) = classify_direction(0x0A00_0002, 2000, 0x0A00_0001, 1000);
        assert_eq!(k1.low_ip, k2.low_ip);
        assert_eq!(k1.high_ip, k2.high_ip);
        assert_eq!(k1.low_port, k2.low_port);
        assert_eq!(k1.high_port, k2.high_port);
        assert!(l1 && !l2);
    }

    #[test]
    fn canonicalization_options_default_matches_plan() {
        let opts = CanonicalizationOptions::default();
        assert_eq!(opts.canonical_iss, 0x1234_5678);
        assert_eq!(opts.canonical_ts_base, 0xAABB_CCDD);
        assert_eq!(opts.canonical_src_mac, [0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
        assert_eq!(opts.canonical_dst_mac, [0x02, 0x00, 0x00, 0x00, 0x00, 0x02]);
    }
}
