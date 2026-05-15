//! IPv4 decode. Operates on the Ethernet payload (slice starting at the IP
//! header). Returns the decoded header or a drop reason. Checksum is
//! verified only when the NIC didn't (caller passes `nic_csum_ok=true` to
//! skip). Fragments are never accepted — spec §6.3 defers IPv4 reassembly.

pub const IPPROTO_ICMP: u8 = 1;
pub const IPPROTO_TCP: u8 = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct L3Decoded {
    pub protocol: u8,
    pub src_ip: u32, // host byte order
    pub dst_ip: u32, // host byte order
    pub header_len: usize,
    pub total_len: usize,
    pub ttl: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum L3Drop {
    Short,            // fewer than 20 bytes
    BadVersion,       // version != 4
    BadHeaderLen,     // IHL < 5 or header extends past slice
    BadTotalLen,      // total_length < header_len or > slice
    CsumBad,          // checksum verify failed
    TtlZero,          // TTL == 0 on ingress (RFC 791; we drop rather than send ICMP)
    Fragment,         // MF=1 or frag_offset != 0
    NotOurs,          // dst_ip != our_ip (and our_ip != 0)
    UnsupportedProto, // protocol is not TCP and not ICMP
}

/// Compute the Internet checksum (RFC 1071) over a disjoint set of byte slices.
/// Folds each chunk in order, carrying an odd-boundary byte across chunk
/// transitions so the result is bit-for-bit identical to folding a single
/// concatenated buffer. A6.5 §7.6: callers pre-build pseudo-headers as
/// stack arrays and pass `&[&pseudo, tcp_hdr, payload]` without allocating.
///
/// PO3 optimization: when no carry is pending at chunk start (the common
/// case — only chunks following an odd-length predecessor see a pending
/// carry), the chunk body is folded 4 bytes per iteration via
/// `chunks_exact(4)` (two 16-bit BE words per group), bounds-check-free.
/// The carry-pending branch keeps the original 2-byte loop unchanged so
/// chunk-boundary semantics are byte-identical to the pre-PO3 fold. `sum`
/// is widened to `u64` so accumulation cannot overflow for any input size;
/// the final end-around-carry fold works unchanged on a u64.
pub fn internet_checksum(chunks: &[&[u8]]) -> u16 {
    // `sum` is widened to `u64` from the pre-PO3 `u32` for RFC 1071 §1
    // correctness at very large inputs. The old `u32` accumulator silently
    // truncated overflow at inputs >128 KB (~32k 16-bit words, when the
    // running sum surpasses `u32::MAX`); RFC 1071 specifies the
    // one's-complement sum with end-around carry — no truncation. A `u64`
    // holds at least 2^48 16-bit-word adds without overflow, comfortably
    // covering any realistic input including a fully-filled IPv4 packet
    // (~65 KB) plus pseudo-header. Production TCP/IP packets are <2 KB so
    // the old `u32` divergence point was unreachable in real traffic;
    // the widening is preventive correctness, not a fix for a hot defect.
    // See `po3_large_input_u64_widening_no_overflow` for the regression
    // guard that locks in the RFC-correct behavior past the old wrap point.
    let mut sum: u64 = 0;
    let mut carry: Option<u8> = None;
    for chunk in chunks {
        // Carry-pending branch: pair the leftover byte with chunk[0], then
        // fall through to the 2-byte loop for the rest of THIS chunk so
        // the boundary semantics are bit-identical to the original fold.
        if let Some(high) = carry.take() {
            let mut i: usize;
            if let Some(&low) = chunk.first() {
                sum = sum.wrapping_add(u16::from_be_bytes([high, low]) as u64);
                i = 1;
            } else {
                carry = Some(high);
                continue;
            }
            while i + 1 < chunk.len() {
                sum = sum.wrapping_add(u16::from_be_bytes([chunk[i], chunk[i + 1]]) as u64);
                i += 2;
            }
            if i < chunk.len() {
                carry = Some(chunk[i]);
            }
            continue;
        }
        // Fast path: no pending carry, so chunk starts on the same parity
        // as a single concatenated buffer would. Fold the bulk 4 bytes at
        // a time via chunks_exact(4) — each iteration adds two 16-bit BE
        // words to `sum`, bounds-check-free since the iterator promises
        // exactly 4 bytes per group. After the bulk, handle the
        // 0/1/2/3-byte tail.
        let mut iter = chunk.chunks_exact(4);
        for group in &mut iter {
            let w0 = u16::from_be_bytes([group[0], group[1]]) as u64;
            let w1 = u16::from_be_bytes([group[2], group[3]]) as u64;
            sum = sum.wrapping_add(w0).wrapping_add(w1);
        }
        let tail = iter.remainder();
        match tail.len() {
            0 => {}
            1 => carry = Some(tail[0]),
            2 => {
                sum = sum.wrapping_add(u16::from_be_bytes([tail[0], tail[1]]) as u64);
            }
            3 => {
                sum = sum.wrapping_add(u16::from_be_bytes([tail[0], tail[1]]) as u64);
                carry = Some(tail[2]);
            }
            _ => unreachable!("chunks_exact(4) remainder is at most 3 bytes"),
        }
    }
    if let Some(tail) = carry {
        sum = sum.wrapping_add((tail as u64) << 8);
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

/// Decode an IPv4 packet. `our_ip` in host byte order; 0 = accept any dst.
/// `nic_csum_ok`: when true the caller promises the NIC's HW csum passed.
pub fn ip_decode(pkt: &[u8], our_ip: u32, nic_csum_ok: bool) -> Result<L3Decoded, L3Drop> {
    if pkt.len() < 20 {
        return Err(L3Drop::Short);
    }
    let version = pkt[0] >> 4;
    if version != 4 {
        return Err(L3Drop::BadVersion);
    }
    let ihl = (pkt[0] & 0x0f) as usize;
    let header_len = ihl * 4;
    if ihl < 5 || header_len > pkt.len() {
        return Err(L3Drop::BadHeaderLen);
    }
    let total_len = u16::from_be_bytes([pkt[2], pkt[3]]) as usize;
    if total_len < header_len || total_len > pkt.len() {
        return Err(L3Drop::BadTotalLen);
    }
    // Fragment detection: the flags+fragoffset field is bytes 6..8; bit 13
    // from the MSB is MF (More Fragments), low 13 bits are the offset.
    let flags_frag = u16::from_be_bytes([pkt[6], pkt[7]]);
    let mf = (flags_frag & 0x2000) != 0;
    let frag_off = flags_frag & 0x1fff;
    if mf || frag_off != 0 {
        return Err(L3Drop::Fragment);
    }
    let ttl = pkt[8];
    if ttl == 0 {
        return Err(L3Drop::TtlZero);
    }
    // Checksum: verify only when NIC didn't. Zero the checksum bytes in a
    // scratch copy and fold — the computed value should equal what's in the
    // header.
    if !nic_csum_ok {
        let mut scratch = [0u8; 60]; // max IP header length
        scratch[..header_len].copy_from_slice(&pkt[..header_len]);
        scratch[10] = 0;
        scratch[11] = 0;
        let computed = internet_checksum(&[&scratch[..header_len]]);
        let stored = u16::from_be_bytes([pkt[10], pkt[11]]);
        if computed != stored {
            return Err(L3Drop::CsumBad);
        }
    }
    let protocol = pkt[9];
    let src_ip = u32::from_be_bytes([pkt[12], pkt[13], pkt[14], pkt[15]]);
    let dst_ip = u32::from_be_bytes([pkt[16], pkt[17], pkt[18], pkt[19]]);
    if our_ip != 0 && dst_ip != our_ip {
        return Err(L3Drop::NotOurs);
    }
    if protocol != IPPROTO_TCP && protocol != IPPROTO_ICMP {
        return Err(L3Drop::UnsupportedProto);
    }
    Ok(L3Decoded {
        protocol,
        src_ip,
        dst_ip,
        header_len,
        total_len,
        ttl,
    })
}

/// RX checksum classification from `mbuf.ol_flags`, per DPDK's 2-bit
/// encoding on the RTE_MBUF_F_RX_IP_CKSUM_MASK / L4_CKSUM_MASK bits.
/// Feature-gated on `hw-offload-rx-cksum`; absent from feature-off builds.
/// See spec §7.
#[cfg(feature = "hw-offload-rx-cksum")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CksumOutcome {
    /// NIC did not verify — software verify required.
    Unknown,
    /// NIC verified and rejected — drop.
    Bad,
    /// NIC verified and accepted — skip software verify.
    Good,
    /// NIC explicitly signaled no verification — software verify required.
    None,
}

/// Map the IP-cksum bits in `ol_flags` (masked by
/// RTE_MBUF_F_RX_IP_CKSUM_MASK) to a `CksumOutcome`. See spec §7.
#[cfg(feature = "hw-offload-rx-cksum")]
pub fn classify_ip_rx_cksum(ol_flags: u64) -> CksumOutcome {
    use crate::dpdk_consts::{
        RTE_MBUF_F_RX_IP_CKSUM_BAD, RTE_MBUF_F_RX_IP_CKSUM_GOOD, RTE_MBUF_F_RX_IP_CKSUM_MASK,
        RTE_MBUF_F_RX_IP_CKSUM_NONE,
    };
    let m = ol_flags & RTE_MBUF_F_RX_IP_CKSUM_MASK;
    if m == RTE_MBUF_F_RX_IP_CKSUM_GOOD {
        CksumOutcome::Good
    } else if m == RTE_MBUF_F_RX_IP_CKSUM_BAD {
        CksumOutcome::Bad
    } else if m == RTE_MBUF_F_RX_IP_CKSUM_NONE {
        CksumOutcome::None
    } else {
        CksumOutcome::Unknown
    }
}

/// Map the L4-cksum bits in `ol_flags` (masked by
/// RTE_MBUF_F_RX_L4_CKSUM_MASK) to a `CksumOutcome`. See spec §7.
#[cfg(feature = "hw-offload-rx-cksum")]
pub fn classify_l4_rx_cksum(ol_flags: u64) -> CksumOutcome {
    use crate::dpdk_consts::{
        RTE_MBUF_F_RX_L4_CKSUM_BAD, RTE_MBUF_F_RX_L4_CKSUM_GOOD, RTE_MBUF_F_RX_L4_CKSUM_MASK,
        RTE_MBUF_F_RX_L4_CKSUM_NONE,
    };
    let m = ol_flags & RTE_MBUF_F_RX_L4_CKSUM_MASK;
    if m == RTE_MBUF_F_RX_L4_CKSUM_GOOD {
        CksumOutcome::Good
    } else if m == RTE_MBUF_F_RX_L4_CKSUM_BAD {
        CksumOutcome::Bad
    } else if m == RTE_MBUF_F_RX_L4_CKSUM_NONE {
        CksumOutcome::None
    } else {
        CksumOutcome::Unknown
    }
}

/// Offload-aware IP decode entry point for the RX path. Consumes
/// `mbuf.ol_flags` to decide whether software IP-cksum verify is
/// needed, and drops+counter-bumps on NIC-reported BAD. When
/// `hw-offload-rx-cksum` is compile-off OR the engine's runtime latch
/// `rx_cksum_offload_active` is false, forwards directly to
/// `ip_decode(.., nic_csum_ok=false)` — always software verify. See
/// spec §7.2 (runtime fallback on non-advertising PMDs).
pub fn ip_decode_offload_aware(
    pkt: &[u8],
    our_ip: u32,
    #[allow(unused_variables)] ol_flags: u64,
    #[allow(unused_variables)] rx_cksum_offload_active: bool,
    #[allow(unused_variables)] counters: &crate::counters::Counters,
) -> Result<L3Decoded, L3Drop> {
    #[cfg(feature = "hw-offload-rx-cksum")]
    {
        if !rx_cksum_offload_active {
            return ip_decode(pkt, our_ip, false);
        }
        use std::sync::atomic::Ordering;
        match classify_ip_rx_cksum(ol_flags) {
            CksumOutcome::Good => ip_decode(pkt, our_ip, true),
            CksumOutcome::Bad => {
                counters
                    .eth
                    .rx_drop_cksum_bad
                    .fetch_add(1, Ordering::Relaxed);
                counters.ip.rx_csum_bad.fetch_add(1, Ordering::Relaxed);
                Err(L3Drop::CsumBad)
            }
            _ => ip_decode(pkt, our_ip, false),
        }
    }
    #[cfg(not(feature = "hw-offload-rx-cksum"))]
    {
        let _ = ol_flags;
        let _ = rx_cksum_offload_active;
        let _ = counters;
        ip_decode(pkt, our_ip, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a valid IPv4 header with an optional wrong-checksum flag.
    fn build_ip_hdr(proto: u8, src: u32, dst: u32, payload_len: usize, bad_csum: bool) -> Vec<u8> {
        let total = 20 + payload_len;
        let mut v = vec![
            0x45, // version 4, IHL 5
            0x00, // DSCP/ECN
            (total >> 8) as u8,
            (total & 0xff) as u8, // total length
            0x00,
            0x01, // identification
            0x40,
            0x00,  // flags=DF, fragment offset 0
            0x40,  // TTL 64
            proto, // protocol
            0x00,
            0x00, // checksum placeholder
        ];
        v.extend_from_slice(&src.to_be_bytes());
        v.extend_from_slice(&dst.to_be_bytes());
        let cksum = internet_checksum(&[&v]);
        v[10] = (cksum >> 8) as u8;
        v[11] = (cksum & 0xff) as u8;
        if bad_csum {
            v[10] ^= 0xff; // corrupt
        }
        v.resize(total, 0);
        v
    }

    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn checksum_folds_correctly() {
        let h = build_ip_hdr(IPPROTO_TCP, 0x0a000001, 0x0a000002, 0, false);
        // Scratch-zero csum bytes, recompute, compare against stored.
        let mut s = h[..20].to_vec();
        s[10] = 0;
        s[11] = 0;
        let computed = internet_checksum(&[&s]);
        let stored = u16::from_be_bytes([h[10], h[11]]);
        assert_eq!(computed, stored);
    }

    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn short_packet_dropped() {
        assert_eq!(ip_decode(&[0u8; 10], 0, true), Err(L3Drop::Short));
    }

    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn bad_version_dropped() {
        let mut h = build_ip_hdr(IPPROTO_TCP, 1, 2, 0, false);
        h[0] = 0x65; // version 6
        assert_eq!(ip_decode(&h, 0, true), Err(L3Drop::BadVersion));
    }

    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn bad_header_len_dropped() {
        let mut h = build_ip_hdr(IPPROTO_TCP, 1, 2, 0, false);
        h[0] = 0x44; // IHL 4
        assert_eq!(ip_decode(&h, 0, true), Err(L3Drop::BadHeaderLen));
    }

    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn bad_total_len_dropped() {
        let mut h = build_ip_hdr(IPPROTO_TCP, 1, 2, 0, false);
        h[2] = 0x00;
        h[3] = 0x10; // total_length=16 < header_len=20
        assert_eq!(ip_decode(&h, 0, true), Err(L3Drop::BadTotalLen));
    }

    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn fragment_dropped_mf() {
        let mut h = build_ip_hdr(IPPROTO_TCP, 1, 2, 0, false);
        h[6] = 0x20; // set MF bit
        assert_eq!(ip_decode(&h, 0, true), Err(L3Drop::Fragment));
    }

    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn fragment_dropped_offset() {
        let mut h = build_ip_hdr(IPPROTO_TCP, 1, 2, 0, false);
        h[6] = 0x00;
        h[7] = 0x01; // offset=1
        assert_eq!(ip_decode(&h, 0, true), Err(L3Drop::Fragment));
    }

    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn ttl_zero_dropped() {
        let mut h = build_ip_hdr(IPPROTO_TCP, 1, 2, 0, false);
        h[8] = 0;
        // need to refresh csum after editing TTL
        h[10] = 0;
        h[11] = 0;
        let cks = internet_checksum(&[&h[..20]]);
        h[10] = (cks >> 8) as u8;
        h[11] = (cks & 0xff) as u8;
        assert_eq!(ip_decode(&h, 0, true), Err(L3Drop::TtlZero));
    }

    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn bad_csum_dropped_when_verifying() {
        let h = build_ip_hdr(IPPROTO_TCP, 1, 2, 0, true);
        assert_eq!(ip_decode(&h, 0, false), Err(L3Drop::CsumBad));
    }

    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn bad_csum_passes_when_nic_ok() {
        let h = build_ip_hdr(IPPROTO_TCP, 1, 2, 0, true);
        assert!(ip_decode(&h, 0, true).is_ok());
    }

    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn not_ours_dropped() {
        let h = build_ip_hdr(IPPROTO_TCP, 1, 2, 0, false);
        assert_eq!(ip_decode(&h, 99, true), Err(L3Drop::NotOurs));
    }

    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn unsupported_proto_dropped() {
        let h = build_ip_hdr(17 /* UDP */, 1, 2, 0, false);
        assert_eq!(ip_decode(&h, 0, true), Err(L3Drop::UnsupportedProto));
    }

    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn tcp_accepted() {
        let h = build_ip_hdr(IPPROTO_TCP, 1, 2, 10, false);
        let d = ip_decode(&h, 0, true).expect("accepted");
        assert_eq!(d.protocol, IPPROTO_TCP);
        assert_eq!(d.header_len, 20);
        assert_eq!(d.total_len, 30);
    }

    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn icmp_accepted() {
        let h = build_ip_hdr(IPPROTO_ICMP, 1, 2, 4, false);
        let d = ip_decode(&h, 0, true).expect("accepted");
        assert_eq!(d.protocol, IPPROTO_ICMP);
    }

    #[cfg(feature = "hw-offload-rx-cksum")]
    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn classify_ip_cksum_from_ol_flags() {
        use crate::dpdk_consts::{
            RTE_MBUF_F_RX_IP_CKSUM_BAD, RTE_MBUF_F_RX_IP_CKSUM_GOOD,
            RTE_MBUF_F_RX_IP_CKSUM_NONE, RTE_MBUF_F_RX_IP_CKSUM_UNKNOWN,
        };
        assert_eq!(
            classify_ip_rx_cksum(RTE_MBUF_F_RX_IP_CKSUM_GOOD),
            CksumOutcome::Good
        );
        assert_eq!(
            classify_ip_rx_cksum(RTE_MBUF_F_RX_IP_CKSUM_BAD),
            CksumOutcome::Bad
        );
        assert_eq!(
            classify_ip_rx_cksum(RTE_MBUF_F_RX_IP_CKSUM_UNKNOWN),
            CksumOutcome::Unknown
        );
        assert_eq!(
            classify_ip_rx_cksum(RTE_MBUF_F_RX_IP_CKSUM_NONE),
            CksumOutcome::None
        );
    }

    #[cfg(feature = "hw-offload-rx-cksum")]
    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn classify_l4_cksum_from_ol_flags() {
        use crate::dpdk_consts::{
            RTE_MBUF_F_RX_L4_CKSUM_BAD, RTE_MBUF_F_RX_L4_CKSUM_GOOD,
            RTE_MBUF_F_RX_L4_CKSUM_NONE, RTE_MBUF_F_RX_L4_CKSUM_UNKNOWN,
        };
        assert_eq!(
            classify_l4_rx_cksum(RTE_MBUF_F_RX_L4_CKSUM_GOOD),
            CksumOutcome::Good
        );
        assert_eq!(
            classify_l4_rx_cksum(RTE_MBUF_F_RX_L4_CKSUM_BAD),
            CksumOutcome::Bad
        );
        assert_eq!(
            classify_l4_rx_cksum(RTE_MBUF_F_RX_L4_CKSUM_UNKNOWN),
            CksumOutcome::Unknown
        );
        assert_eq!(
            classify_l4_rx_cksum(RTE_MBUF_F_RX_L4_CKSUM_NONE),
            CksumOutcome::None
        );
    }

    // ------------------------------------------------------------------
    // PO3 — `internet_checksum` 4-byte fold optimization safety net.
    //
    // `internet_checksum_ref` is the simple byte-pair-by-byte fold —
    // structurally identical to the pre-PO3 implementation, BUT with
    // `sum` widened to `u64` to match the post-PO3 production code's
    // RFC 1071 correctness at very large inputs. The pre-PO3 `u32`
    // accumulator silently truncated overflow at >128 KB inputs; that
    // was a latent bug the PO3 widening fixed. Keeping the reference's
    // `sum` at `u32` would mean this property test verifies "new
    // optimized matches OLD buggy" — passing only because the test
    // generates inputs (≤1 KB total) well below the divergence point.
    // Using `u64` here makes the property test a meaningful RFC 1071
    // equivalence check: two independent algorithms (4-byte fast path
    // vs. byte-pair-by-byte) compared against the same correctness
    // standard. The lock-in for the >128 KB widening point itself is
    // in `po3_large_input_u64_widening_no_overflow` below.
    // ------------------------------------------------------------------

    /// Reference fold: simple byte-pair-by-byte algorithm with a `u64`
    /// accumulator, used as the safety-net oracle for the 4-byte fast
    /// path. `sum: u64` matches the post-PO3 production accumulator so
    /// the comparison is a RFC 1071 §1 equivalence check, not a parity
    /// check against the pre-PO3 `u32` algorithm (which truncated
    /// overflow at very large inputs).
    #[cfg(test)]
    fn internet_checksum_ref(chunks: &[&[u8]]) -> u16 {
        let mut sum: u64 = 0;
        let mut carry: Option<u8> = None;
        for chunk in chunks {
            let mut i = 0usize;
            if let Some(high) = carry.take() {
                if let Some(&low) = chunk.first() {
                    sum = sum.wrapping_add(u16::from_be_bytes([high, low]) as u64);
                    i = 1;
                } else {
                    carry = Some(high);
                    continue;
                }
            }
            while i + 1 < chunk.len() {
                sum = sum.wrapping_add(u16::from_be_bytes([chunk[i], chunk[i + 1]]) as u64);
                i += 2;
            }
            if i < chunk.len() {
                carry = Some(chunk[i]);
            }
        }
        if let Some(tail) = carry {
            sum = sum.wrapping_add((tail as u64) << 8);
        }
        while sum >> 16 != 0 {
            sum = (sum & 0xffff) + (sum >> 16);
        }
        !(sum as u16)
    }

    #[cfg_attr(miri, ignore = "long property test; oracle algorithm")]
    #[test]
    fn po3_edge_cases_empty_list() {
        // Empty chunks list: no bytes, sum=0, fold no-op, complement = 0xffff.
        assert_eq!(internet_checksum(&[]), 0xffff);
        assert_eq!(internet_checksum(&[]), internet_checksum_ref(&[]));
    }

    #[cfg_attr(miri, ignore = "long property test; oracle algorithm")]
    #[test]
    fn po3_edge_cases_empty_chunks() {
        // List with only empty chunks must be a no-op.
        assert_eq!(internet_checksum(&[&[]]), 0xffff);
        assert_eq!(internet_checksum(&[&[], &[], &[]]), 0xffff);
        assert_eq!(internet_checksum(&[&[], &[1, 2, 3], &[]]), internet_checksum_ref(&[&[], &[1, 2, 3], &[]]));
    }

    #[cfg_attr(miri, ignore = "long property test; oracle algorithm")]
    #[test]
    fn po3_edge_cases_single_byte() {
        // Single odd byte: contributes as 0xAB00, fold, complement.
        for b in [0x00u8, 0x01, 0x7f, 0x80, 0xab, 0xff] {
            let cs_opt = internet_checksum(&[&[b]]);
            let cs_ref = internet_checksum_ref(&[&[b]]);
            assert_eq!(cs_opt, cs_ref, "single byte 0x{:02x}", b);
        }
    }

    #[cfg_attr(miri, ignore = "long property test; oracle algorithm")]
    #[test]
    fn po3_edge_cases_small_chunk_sizes() {
        // Hand-picked: 1/2/3/4/5/6/7/8-byte chunks — exercises every
        // chunks_exact(4) remainder length (0/1/2/3) and the head
        // 4-byte group boundary.
        let patterns: [&[u8]; 9] = [
            &[],
            &[0xde],
            &[0xde, 0xad],
            &[0xde, 0xad, 0xbe],
            &[0xde, 0xad, 0xbe, 0xef],
            &[0xde, 0xad, 0xbe, 0xef, 0xfe],
            &[0xde, 0xad, 0xbe, 0xef, 0xfe, 0xed],
            &[0xde, 0xad, 0xbe, 0xef, 0xfe, 0xed, 0xfa],
            &[0xde, 0xad, 0xbe, 0xef, 0xfe, 0xed, 0xfa, 0xce],
        ];
        for p in patterns {
            assert_eq!(
                internet_checksum(&[p]),
                internet_checksum_ref(&[p]),
                "len {}",
                p.len()
            );
        }
    }

    #[cfg_attr(miri, ignore = "long property test; oracle algorithm")]
    #[test]
    fn po3_edge_cases_odd_then_even_boundary() {
        // CRITICAL boundary case: odd-length chunk followed by another
        // chunk — the carry byte from chunk 1 pairs with chunk 2's first
        // byte. This is the path that bypasses the 4-byte fast path
        // because `carry` is pending at chunk 2's start.
        let c1: &[u8] = &[0xa1]; // 1 byte → leaves carry
        let c2: &[u8] = &[0xb2, 0xc3, 0xd4, 0xe5]; // 4 bytes
        assert_eq!(
            internet_checksum(&[c1, c2]),
            internet_checksum_ref(&[c1, c2])
        );
        // Carry pairs with c2[0] = 0xb2 → 0xa1b2, then 2-byte loop folds
        // c2[1..3] = (0xc3,0xd4), and c2[4]=0xe5 leaves a new carry.
        let c3: &[u8] = &[0xb2, 0xc3, 0xd4, 0xe5, 0xf6]; // 5 bytes
        assert_eq!(
            internet_checksum(&[c1, c3]),
            internet_checksum_ref(&[c1, c3])
        );
        // Chain of three odd-length chunks.
        let a: &[u8] = &[0x11];
        let b: &[u8] = &[0x22, 0x33, 0x44];
        let c: &[u8] = &[0x55, 0x66, 0x77, 0x88, 0x99];
        assert_eq!(
            internet_checksum(&[a, b, c]),
            internet_checksum_ref(&[a, b, c])
        );
    }

    #[cfg_attr(miri, ignore = "long property test; oracle algorithm")]
    #[test]
    fn po3_edge_cases_three_byte_chunk_pairs() {
        // 3-byte chunks: chunks_exact(4) yields zero groups, remainder=3
        // → the `3` arm of the tail match runs.
        let c: &[u8] = &[0xaa, 0xbb, 0xcc];
        assert_eq!(internet_checksum(&[c]), internet_checksum_ref(&[c]));
        // 3+3=6 bytes: first chunk leaves carry, second chunk starts
        // with carry-pending so it takes the 2-byte path.
        let d: &[u8] = &[0x11, 0x22, 0x33];
        let e: &[u8] = &[0x44, 0x55, 0x66];
        assert_eq!(
            internet_checksum(&[d, e]),
            internet_checksum_ref(&[d, e])
        );
    }

    #[cfg_attr(miri, ignore = "131 KB allocation; slow under miri")]
    #[test]
    fn po3_large_input_u64_widening_no_overflow() {
        // 131,076 bytes = 65,538 BE words of 0xFFFF — slightly past the
        // pre-PO3 u32 wrap point (u32::MAX / 0xFFFF + 1 ≈ 65538 iters).
        //
        // Hand-computed RFC 1071 fold (u64 accumulator, no truncation):
        //   raw_sum = 65538 * 0xFFFF = 0x1_0000_FFFE
        //   fold #1: (0x1_0000_FFFE & 0xFFFF) + (0x1_0000_FFFE >> 16)
        //          = 0xFFFE + 0x10000 = 0x1_FFFE
        //   fold #2: (0x1_FFFE & 0xFFFF) + (0x1_FFFE >> 16)
        //          = 0xFFFE + 0x1 = 0xFFFF
        //   sum >> 16 == 0, loop exits.
        //   !0xFFFF = 0x0000.
        //
        // The pre-PO3 u32 accumulator would have wrapped exactly once
        // mid-accumulation (at iteration 65538), leaving 0x0000_FFFE in
        // the u32, folding to 0xFFFE, and returning !0xFFFE = 0x0001.
        // That divergence (0x0001 vs. 0x0000) is the latent u32 overflow
        // bug PO3's u64 widening fixed. Production IPv4 packets cap at
        // 65,535 bytes total — the divergence point was unreachable in
        // real traffic. This test locks in the RFC-correct u64 behavior
        // so any future change reintroducing u32 saturation fails here.
        let big: Vec<u8> = vec![0xFFu8; 65538 * 2];
        let chunks: [&[u8]; 1] = [big.as_slice()];
        let got = internet_checksum(&chunks);
        assert_eq!(got, 0x0000, "u64 RFC 1071 fold result");
        // And the byte-pair-by-byte reference (also u64) must agree —
        // two independent algorithms, one correctness standard.
        let got_ref = internet_checksum_ref(&chunks);
        assert_eq!(got_ref, 0x0000, "u64 reference fold result");
        assert_eq!(got, got_ref);
    }

    /// Deterministic xorshift64* PRNG — no external dependency.
    /// Stream state lives in the closure caller.
    #[inline]
    fn xorshift64star_next(state: &mut u64) -> u64 {
        let mut x = *state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        *state = x;
        x.wrapping_mul(0x2545F4914F6CDD1D)
    }

    #[cfg_attr(miri, ignore = "long property test")]
    #[test]
    fn po3_property_random_chunks_match_reference() {
        // 5000 random configurations: 1-5 chunks, each 0-200 bytes,
        // random byte content. Assert optimized == reference for every
        // configuration.
        let mut rng: u64 = 0xdeadbeefcafef00d;
        for iter in 0..5000 {
            let r = xorshift64star_next(&mut rng);
            let n_chunks = ((r & 0x7) % 5) as usize + 1; // 1..=5
            let mut bufs: Vec<Vec<u8>> = Vec::with_capacity(n_chunks);
            for _ in 0..n_chunks {
                let r2 = xorshift64star_next(&mut rng);
                let len = (r2 % 201) as usize; // 0..=200
                let mut buf = Vec::with_capacity(len);
                for _ in 0..len {
                    let rb = xorshift64star_next(&mut rng);
                    buf.push(rb as u8);
                }
                bufs.push(buf);
            }
            let slices: Vec<&[u8]> = bufs.iter().map(|v| v.as_slice()).collect();
            let opt = internet_checksum(&slices);
            let reference = internet_checksum_ref(&slices);
            assert_eq!(
                opt, reference,
                "iter {} diverged: chunks={:?}",
                iter,
                bufs.iter().map(|v| v.len()).collect::<Vec<_>>()
            );
        }
    }

    #[cfg_attr(miri, ignore = "long property test")]
    #[test]
    fn po3_property_random_includes_carry_boundaries() {
        // Targeted: force odd-length first chunks so the carry-pending
        // boundary path is exercised heavily. 5000 iters.
        let mut rng: u64 = 0x0123456789abcdef;
        for iter in 0..5000 {
            let r = xorshift64star_next(&mut rng);
            let n_chunks = ((r & 0x7) % 4) as usize + 2; // 2..=5
            let mut bufs: Vec<Vec<u8>> = Vec::with_capacity(n_chunks);
            for idx in 0..n_chunks {
                let r2 = xorshift64star_next(&mut rng);
                // Make ALL chunks odd-length so every transition is a
                // carry-pending boundary. Mix in occasional 0-length.
                let force_zero = (r2 & 0xff) < 16;
                let len = if force_zero {
                    0
                } else {
                    let raw = (r2 % 100) as usize;
                    raw * 2 + 1 // always odd
                };
                let _ = idx;
                let mut buf = Vec::with_capacity(len);
                for _ in 0..len {
                    let rb = xorshift64star_next(&mut rng);
                    buf.push(rb as u8);
                }
                bufs.push(buf);
            }
            let slices: Vec<&[u8]> = bufs.iter().map(|v| v.as_slice()).collect();
            let opt = internet_checksum(&slices);
            let reference = internet_checksum_ref(&slices);
            assert_eq!(
                opt, reference,
                "carry-iter {} diverged: chunks={:?}",
                iter,
                bufs.iter().map(|v| v.len()).collect::<Vec<_>>()
            );
        }
    }
}
