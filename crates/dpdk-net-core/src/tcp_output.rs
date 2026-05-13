//! TCP segment builders. Every builder emits a complete Ethernet + IPv4 +
//! TCP frame with optional TCP options (MSS / WS / SACK-permitted / TS /
//! SACK blocks). IPv4 header checksum is computed in software; TCP
//! checksum uses the pseudo-header form per RFC 9293 §3.1.
//!
//! Option encoding is delegated to `tcp_options::TcpOpts::encode` (canonical
//! order + NOP-word-alignment).

use crate::l2::{ETHERTYPE_IPV4, ETH_HDR_LEN};
use crate::l3_ip::{internet_checksum, IPPROTO_TCP};
use crate::tcp_options::TcpOpts;

pub const TCP_HDR_MIN: usize = 20;
pub const IPV4_HDR_MIN: usize = 20;
pub const FRAME_HDRS_MIN: usize = ETH_HDR_LEN + IPV4_HDR_MIN + TCP_HDR_MIN;

pub const TCP_FIN: u8 = 0x01;
pub const TCP_SYN: u8 = 0x02;
pub const TCP_RST: u8 = 0x04;
pub const TCP_PSH: u8 = 0x08;
pub const TCP_ACK: u8 = 0x10;
/// URG (urgent pointer) flag. Stage 1 does not support URG; inbound URG
/// segments are dropped and counted via `tcp.rx_urgent_dropped` (A4
/// cross-phase backfill — spec §9.1.1 / plan task 19).
pub const TCP_URG: u8 = 0x20;

pub struct SegmentTx<'a> {
    pub src_mac: [u8; 6],
    pub dst_mac: [u8; 6],
    pub src_ip: u32,
    pub dst_ip: u32,
    pub src_port: u16,
    pub dst_port: u16,
    pub seq: u32,
    pub ack: u32,
    pub flags: u8,
    pub window: u16,
    /// Any combination of options; use `TcpOpts::default()` for none.
    pub options: TcpOpts,
    pub payload: &'a [u8],
}

pub fn build_segment(seg: &SegmentTx, out: &mut [u8]) -> Option<usize> {
    // The single-mbuf case: payload is inline in `seg.payload` — the
    // checksum is computed over the header + the same bytes we write
    // to `out`, and the IPv4 total-length / TCP pseudo-header length
    // fields use `seg.payload.len()`.
    build_segment_inner(seg, seg.payload, seg.payload.len() as u32, out, CsumStrategy::Full)
}

/// Same wire-bytes output as `build_segment` followed by
/// `tx_offload_rewrite_cksums`, but written in one pass — the IPv4 cksum
/// field is set to 0 (NIC computes at wire time) and the TCP cksum field
/// is set to the pseudo-header-only fold (PMD folds in TCP header +
/// payload at wire time when `RTE_MBUF_F_TX_TCP_CKSUM` is set on the
/// mbuf via `tx_offload_finalize`).
///
/// PO4 (perf optimisation): on the `tx_cksum_offload_active == true`
/// path the full TCP payload fold that `build_segment` performs is dead
/// work — `tx_offload_finalize` runs immediately after and overwrites
/// the TCP cksum with the pseudo-only value (and zeros the IPv4 cksum).
/// `build_segment_offload` produces that final post-finalize cksum
/// state directly, skipping the full payload fold (~250-400 ns at MSS).
///
/// Wire-equivalence invariant: after `build_segment_offload(seg, out)`
/// followed by `tx_offload_rewrite_cksums(out, ...)`, the bytes in
/// `out[..n]` are bit-identical to the bytes produced by
/// `build_segment(seg, out)` followed by `tx_offload_rewrite_cksums`
/// (verified by the
/// `build_segment_offload_wire_equivalent_to_build_segment_plus_rewrite`
/// unit test). `tx_offload_finalize` STILL runs on the offload-active
/// path — its mbuf flag set + l2/l3/l4 length set are essential and are
/// not provided by `build_segment_offload`. The redundant second write
/// of the same TCP cksum + IPv4 cksum fields is deliberately left in
/// place; the cost (two 16-bit stores) is negligible and removing it
/// would split offload-active correctness across two functions.
pub fn build_segment_offload(seg: &SegmentTx, out: &mut [u8]) -> Option<usize> {
    build_segment_inner(
        seg,
        seg.payload,
        seg.payload.len() as u32,
        out,
        CsumStrategy::Offload,
    )
}

/// Selects the cksum-write strategy used by `build_segment_inner`.
///
/// * `Full` — SW IPv4 cksum + SW full TCP cksum (pseudo-header + TCP
///   header + payload fold). Used by `build_segment` /
///   `build_retrans_header` on the non-offload path; the NIC transmits
///   the bytes unchanged.
/// * `Offload` — IPv4 cksum field = 0; TCP cksum field = pseudo-header
///   only. Skips the full payload fold; the NIC computes both checksums
///   at wire time after `tx_offload_finalize` sets the mbuf flags.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CsumStrategy {
    Full,
    Offload,
}

/// Write L2+L3+TCP headers for a retransmit into `out`. The payload
/// itself lives in a separately-chained data mbuf downstream, so the
/// header mbuf contains NO payload bytes — but the IPv4 `total length`
/// field, the TCP pseudo-header checksum length, and the TCP checksum
/// itself MUST reflect the full on-wire segment.
///
/// Spec §6.5 "retransmit primitive": alloc fresh hdr mbuf, chain to the
/// original data mbuf, never edit the in-flight mbuf in place.
///
/// * `seg.payload` MUST be empty (`&[]`) — payload writes go into the
///   chained data mbuf, not `out`.
/// * `payload_for_csum` is the ACTUAL payload byte slice, read by the
///   caller from the held data mbuf via `shim_rte_pktmbuf_data` and
///   passed in only so the checksum folds in the payload contribution.
///
/// Returns the number of bytes written (header length only), or `None`
/// if `out` is too small or `payload_for_csum.len() > u16::MAX`.
///
/// Invariant: `build_retrans_header(seg_empty, p, out)` produces the
/// same L2/L3/TCP header bytes as `build_segment(seg_with_p, out)` —
/// specifically the IP total-length, TCP pseudo-header length, and TCP
/// checksum all match. Verified by unit test
/// `build_retrans_header_matches_build_segment_header_prefix`.
pub fn build_retrans_header(
    seg: &SegmentTx,
    payload_for_csum: &[u8],
    out: &mut [u8],
) -> Option<usize> {
    debug_assert!(
        seg.payload.is_empty(),
        "build_retrans_header expects seg.payload == &[] — payload lives in chained data mbuf"
    );
    if payload_for_csum.len() > u16::MAX as usize {
        return None;
    }
    build_segment_inner(
        seg,
        payload_for_csum,
        payload_for_csum.len() as u32,
        out,
        CsumStrategy::Full,
    )
}

/// Offload-aware sibling of `build_retrans_header`. Same wire-bytes
/// output as `build_retrans_header(seg, payload_bytes, out)` followed by
/// `tx_offload_rewrite_cksums`, but written in one pass with no SW
/// payload fold.
///
/// PO6 (perf optimisation): on the `tx_cksum_offload_active == true`
/// retransmit path, the full TCP payload fold that `build_retrans_header`
/// performs over the chained-data-mbuf bytes is dead work — the NIC
/// re-folds those same bytes at wire time. `build_retrans_header_offload`
/// produces the post-finalize cksum state directly, skipping BOTH the
/// per-byte payload fold AND the `shim_rte_pktmbuf_data` read that
/// fetches the payload bytes out of the chained data mbuf. Stronger than
/// PO4 for the send_bytes hot path: this skips the FFI shim call too.
///
/// `payload_len_for_framing` is the wire-side payload byte count (the
/// `entry.len` value the caller passes in). It is used ONLY for the
/// IPv4 total-length and TCP pseudo-header length fields — there are
/// NO payload bytes folded, so the caller does NOT need to read the
/// chained data mbuf's bytes.
///
/// `seg.payload` MUST be empty (`&[]`) — payload writes go into the
/// chained data mbuf, not `out`.
///
/// Returns the number of bytes written (header length only), or `None`
/// if `out` is too small or `payload_len_for_framing > u16::MAX`.
///
/// Wire-equivalence invariant: after
/// `build_retrans_header_offload(seg, payload_len, out)` followed by
/// `tx_offload_rewrite_cksums(out, ...)`, the bytes in `out[..n]` are
/// bit-identical to the bytes produced by
/// `build_retrans_header(seg, real_payload, out)` followed by
/// `tx_offload_rewrite_cksums` (verified by the
/// `build_retrans_header_offload_wire_equivalent_to_build_retrans_header_plus_rewrite`
/// unit test).
pub fn build_retrans_header_offload(
    seg: &SegmentTx,
    payload_len_for_framing: u32,
    out: &mut [u8],
) -> Option<usize> {
    debug_assert!(
        seg.payload.is_empty(),
        "build_retrans_header_offload expects seg.payload == &[] — payload lives in chained data mbuf"
    );
    if payload_len_for_framing > u16::MAX as u32 {
        return None;
    }
    // Pass an empty payload_for_csum slice — under CsumStrategy::Offload
    // the inner builder uses only `declared_payload_len` for the IPv4
    // total-length + TCP pseudo-header length fields and never folds
    // payload bytes, so the slice contents are irrelevant.
    build_segment_inner(
        seg,
        &[],
        payload_len_for_framing,
        out,
        CsumStrategy::Offload,
    )
}

/// Shared implementation for `build_segment` and `build_retrans_header`.
///
/// * `seg.payload` is what actually gets WRITTEN to `out` after the TCP
///   header (may be empty for the retransmit-header case).
/// * `payload_for_csum` is what gets FOLDED into the TCP checksum. In
///   the single-mbuf case it is identical to `seg.payload`. In the
///   retransmit case it is the chained-data mbuf's bytes (so the
///   checksum matches what `build_segment` would produce on the
///   original TX).
/// * `declared_payload_len` is the IPv4 total-length / TCP pseudo-header
///   length field (always equals `payload_for_csum.len()` via the two
///   wrappers, but kept as a separate arg for clarity).
fn build_segment_inner(
    seg: &SegmentTx,
    payload_for_csum: &[u8],
    declared_payload_len: u32,
    out: &mut [u8],
    csum_strategy: CsumStrategy,
) -> Option<usize> {
    let opts_len = seg.options.encoded_len();
    let tcp_hdr_len = TCP_HDR_MIN + opts_len;
    // Bytes we actually write to `out`: headers + (possibly empty) payload.
    let total_written = ETH_HDR_LEN + IPV4_HDR_MIN + tcp_hdr_len + seg.payload.len();
    if out.len() < total_written {
        return None;
    }
    // Guard against a bogus declared_payload_len.
    let total_ip_len_u32 = (IPV4_HDR_MIN + tcp_hdr_len) as u32 + declared_payload_len;
    if total_ip_len_u32 > u16::MAX as u32 {
        return None;
    }

    // Ethernet
    out[0..6].copy_from_slice(&seg.dst_mac);
    out[6..12].copy_from_slice(&seg.src_mac);
    out[12..14].copy_from_slice(&ETHERTYPE_IPV4.to_be_bytes());

    // IPv4 — total-length reflects the on-wire size (header + declared payload).
    let ip_start = ETH_HDR_LEN;
    let ip = &mut out[ip_start..ip_start + IPV4_HDR_MIN];
    let total_ip_len = total_ip_len_u32 as u16;
    ip[0] = 0x45;
    ip[1] = 0x00;
    ip[2..4].copy_from_slice(&total_ip_len.to_be_bytes());
    ip[4..6].copy_from_slice(&0x0000u16.to_be_bytes());
    ip[6..8].copy_from_slice(&0x4000u16.to_be_bytes());
    ip[8] = 64;
    ip[9] = IPPROTO_TCP;
    ip[10..12].copy_from_slice(&0x0000u16.to_be_bytes());
    ip[12..16].copy_from_slice(&seg.src_ip.to_be_bytes());
    ip[16..20].copy_from_slice(&seg.dst_ip.to_be_bytes());
    // PO4: on the offload path the NIC computes the IPv4 cksum at wire
    // time (caller sets `RTE_MBUF_F_TX_IP_CKSUM` via `tx_offload_finalize`);
    // the SW value here would be overwritten with zeros anyway.
    match csum_strategy {
        CsumStrategy::Full => {
            let ip_csum = internet_checksum(&[&out[ip_start..ip_start + IPV4_HDR_MIN]]);
            out[ip_start + 10] = (ip_csum >> 8) as u8;
            out[ip_start + 11] = (ip_csum & 0xff) as u8;
        }
        CsumStrategy::Offload => {
            // IPv4 cksum field already zero from the `0x0000u16` write
            // at ip[10..12] above. No-op kept explicit so the offload
            // contract is obvious at the call site.
        }
    }

    // TCP header + options + (possibly empty) payload
    let tcp_start = ip_start + IPV4_HDR_MIN;
    let th = &mut out[tcp_start..tcp_start + tcp_hdr_len];
    th[0..2].copy_from_slice(&seg.src_port.to_be_bytes());
    th[2..4].copy_from_slice(&seg.dst_port.to_be_bytes());
    th[4..8].copy_from_slice(&seg.seq.to_be_bytes());
    th[8..12].copy_from_slice(&seg.ack.to_be_bytes());
    th[12] = ((tcp_hdr_len / 4) as u8) << 4;
    th[13] = seg.flags;
    th[14..16].copy_from_slice(&seg.window.to_be_bytes());
    th[16..18].copy_from_slice(&0u16.to_be_bytes());
    th[18..20].copy_from_slice(&0u16.to_be_bytes());
    if opts_len > 0 {
        seg.options
            .encode(&mut th[TCP_HDR_MIN..TCP_HDR_MIN + opts_len])
            .expect("pre-sized exactly; encode must fit");
    }

    let payload_start = tcp_start + tcp_hdr_len;
    out[payload_start..payload_start + seg.payload.len()].copy_from_slice(seg.payload);

    // TCP checksum.
    // * Full: pseudo-header + TCP header + payload (folded from
    //   `payload_for_csum`, which comes from either `seg.payload` or the
    //   chained-mbuf bytes).
    // * Offload: pseudo-header only. The PMD folds in TCP header +
    //   payload at wire time when `RTE_MBUF_F_TX_TCP_CKSUM` is set on
    //   the mbuf via `tx_offload_finalize`. This SKIPS the per-byte
    //   payload fold — the PO4 optimisation.
    //
    // `tcp_seg_len` in the pseudo-header matches `declared_payload_len`
    // + header length under both strategies.
    let tcp_seg_len = tcp_hdr_len as u32 + declared_payload_len;
    let csum = match csum_strategy {
        CsumStrategy::Full => tcp_checksum_split(
            seg.src_ip,
            seg.dst_ip,
            tcp_seg_len,
            &out[tcp_start..tcp_start + tcp_hdr_len],
            payload_for_csum,
        ),
        CsumStrategy::Offload => {
            tcp_pseudo_header_checksum(seg.src_ip, seg.dst_ip, tcp_seg_len)
        }
    };
    out[tcp_start + 16] = (csum >> 8) as u8;
    out[tcp_start + 17] = (csum & 0xff) as u8;

    Some(total_written)
}

/// Pseudo-header checksum per RFC 9293 §3.1. Reuses `internet_checksum`
/// by folding a scratch buffer of pseudo-header + tcp segment bytes.
#[cfg(test)]
fn tcp_checksum(src_ip: u32, dst_ip: u32, tcp_seg_len: u32, tcp_bytes: &[u8]) -> u16 {
    tcp_checksum_split(src_ip, dst_ip, tcp_seg_len, tcp_bytes, &[])
}

/// Split-buffer variant: checksum = fold(pseudo-header || tcp_header_bytes
/// || payload_bytes). Used for the retransmit path where the TCP header
/// sits in the header mbuf but the payload lives in a separate chained
/// data mbuf. For the inline `build_segment` case the caller passes
/// `tcp_header_bytes` = the TCP header (including options) and
/// `payload_bytes` = `seg.payload`; the two-call result matches the old
/// single-buffer checksum bit-for-bit.
fn tcp_checksum_split(
    src_ip: u32,
    dst_ip: u32,
    tcp_seg_len: u32,
    tcp_header_bytes: &[u8],
    payload_bytes: &[u8],
) -> u16 {
    // Pseudo-header: src_ip(4) + dst_ip(4) + zero(1) + proto(1) + tcp_len(2).
    // Built on the stack; fold pseudo-header + TCP header + payload as a
    // slice-of-slices via streaming internet_checksum.
    let mut pseudo = [0u8; 12];
    pseudo[0..4].copy_from_slice(&src_ip.to_be_bytes());
    pseudo[4..8].copy_from_slice(&dst_ip.to_be_bytes());
    pseudo[8] = 0;
    pseudo[9] = IPPROTO_TCP;
    pseudo[10..12].copy_from_slice(&(tcp_seg_len as u16).to_be_bytes());
    internet_checksum(&[&pseudo, tcp_header_bytes, payload_bytes])
}

/// Pseudo-header-only TCP checksum per RFC 9293 §3.1. Used by A-HW's
/// TX offload path: software writes ONLY the 12-byte pseudo-header
/// fold into the TCP cksum field; the PMD folds in TCP header +
/// payload at wire time when `RTE_MBUF_F_TX_TCP_CKSUM` is set.
///
/// `tcp_seg_len` is the pseudo-header `tcp_length` field: the sum of
/// header-bytes and payload-bytes on the wire. For a 20-byte TCP header
/// with N bytes payload, tcp_seg_len = 20 + N.
pub fn tcp_pseudo_header_checksum(src_ip: u32, dst_ip: u32, tcp_seg_len: u32) -> u16 {
    debug_assert!(
        tcp_seg_len <= u16::MAX as u32,
        "tcp_seg_len {tcp_seg_len} exceeds u16::MAX — pseudo-header tcp_length field is 16 bits (IPv4 total-length bound)"
    );
    let mut buf = [0u8; 12];
    buf[0..4].copy_from_slice(&src_ip.to_be_bytes());
    buf[4..8].copy_from_slice(&dst_ip.to_be_bytes());
    buf[8] = 0;
    buf[9] = IPPROTO_TCP;
    buf[10..12].copy_from_slice(&(tcp_seg_len as u16).to_be_bytes());
    internet_checksum(&[&buf])
}

/// A-HW Task 7 pure-function helper. Rewrites the TCP and IPv4 checksum
/// fields inside a full Ethernet+IPv4+TCP frame for the TX-offload path:
///   - TCP cksum (frame bytes `ETH_HDR_LEN + IPV4_HDR_MIN + 16..+18`)
///     is overwritten with the pseudo-header-only fold from
///     `tcp_pseudo_header_checksum`.
///   - IPv4 cksum (frame bytes `ETH_HDR_LEN + 10..+12`) is zeroed.
///
/// `frame_bytes` must start at the Ethernet header and be at least
/// `ETH_HDR_LEN + IPV4_HDR_MIN + TCP_HDR_MIN` long — the caller always
/// has a full segment because `build_segment` / `build_retrans_header`
/// ran first. Returns `false` when the slice is too short (defensive;
/// production call sites never hit this branch).
///
/// `tcp_hdr_len` is the TCP header length including options (>=20).
/// `payload_for_csum_len` is the payload byte count that will ship on
/// the wire (= seg.payload.len() for build_segment; the chained data
/// mbuf's data_len for build_retrans_header).
///
/// Split out from `tx_offload_finalize` so unit tests can exercise the
/// memory-rewrite logic against a plain `&mut [u8]` without constructing
/// an opaque `rte_mbuf`. Spec §6.2.
#[cfg(feature = "hw-offload-tx-cksum")]
pub fn tx_offload_rewrite_cksums(
    frame_bytes: &mut [u8],
    src_ip: u32,
    dst_ip: u32,
    tcp_hdr_len: usize,
    payload_for_csum_len: u32,
) -> bool {
    let min_len = ETH_HDR_LEN + IPV4_HDR_MIN + TCP_HDR_MIN;
    if frame_bytes.len() < min_len {
        return false;
    }
    let pseudo_len = (tcp_hdr_len as u32).wrapping_add(payload_for_csum_len);
    let pseudo = tcp_pseudo_header_checksum(src_ip, dst_ip, pseudo_len);
    let tcp_cksum_off = ETH_HDR_LEN + IPV4_HDR_MIN + 16;
    frame_bytes[tcp_cksum_off] = (pseudo >> 8) as u8;
    frame_bytes[tcp_cksum_off + 1] = (pseudo & 0xff) as u8;
    let ip_cksum_off = ETH_HDR_LEN + 10;
    frame_bytes[ip_cksum_off] = 0;
    frame_bytes[ip_cksum_off + 1] = 0;
    true
}

/// A-HW TX offload finalizer. When `offload_active == true` AND the
/// `hw-offload-tx-cksum` feature is compiled in:
///   1. Sets `mbuf.ol_flags |= RTE_MBUF_F_TX_IPV4 | RTE_MBUF_F_TX_IP_CKSUM
///      | RTE_MBUF_F_TX_TCP_CKSUM`.
///   2. Sets `mbuf.l2_len = 14`, `mbuf.l3_len = 20`, `mbuf.l4_len = tcp_hdr_len`.
///   3. Overwrites the TCP checksum field with the pseudo-header-only
///      fold (the PMD folds in TCP header + payload at wire time).
///   4. Zeros the IPv4 header checksum field (PMD computes it).
///
/// When `offload_active == false` OR the feature is compile-off: no-op.
/// The caller's `build_segment` / `build_retrans_header` already produced
/// software full-fold TCP + IPv4 checksums; the NIC transmits exactly
/// those bytes.
///
/// `payload_for_csum_len` is the payload byte count that will ship on
/// the wire — `seg.payload.len() as u32` for `build_segment` callers,
/// the chained data mbuf's `data_len` for `build_retrans_header`.
///
/// # Safety
/// `mbuf` must be a valid pointer to a live `rte_mbuf` whose data buffer
/// contains at least ETH(14) + IPv4(20) + TCP-header bytes already
/// populated by `build_segment` (or `build_retrans_header` for the
/// header mbuf of a chained retransmit). Caller must hold exclusive
/// access to the mbuf for the duration of this call. In the hot path
/// this is satisfied by the ownership rules around build_segment +
/// rte_eth_tx_burst: the mbuf was just freshly allocated from a
/// per-engine mempool and no other code has a pointer to it yet.
///
/// Spec §6.2.
#[cfg(feature = "hw-offload-tx-cksum")]
pub unsafe fn tx_offload_finalize(
    mbuf: *mut dpdk_net_sys::rte_mbuf,
    seg: &SegmentTx,
    payload_for_csum_len: u32,
    offload_active: bool,
) {
    if !offload_active || mbuf.is_null() {
        return;
    }
    use crate::dpdk_consts::{
        RTE_MBUF_F_TX_IP_CKSUM, RTE_MBUF_F_TX_IPV4, RTE_MBUF_F_TX_TCP_CKSUM,
    };
    let opts_len = seg.options.encoded_len();
    let tcp_hdr_len = TCP_HDR_MIN + opts_len;

    // rte_mbuf is opaque to bindgen (packed anonymous unions), so the
    // ol_flags / l2/l3/l4_len metadata goes through sys-crate shims.
    // Safety: `mbuf` is a valid pointer per the caller's contract.
    unsafe {
        dpdk_net_sys::shim_rte_mbuf_or_ol_flags(
            mbuf,
            RTE_MBUF_F_TX_IPV4 | RTE_MBUF_F_TX_IP_CKSUM | RTE_MBUF_F_TX_TCP_CKSUM,
        );
        dpdk_net_sys::shim_rte_mbuf_set_tx_lens(
            mbuf,
            ETH_HDR_LEN as u16,
            IPV4_HDR_MIN as u16,
            tcp_hdr_len as u16,
        );
    }

    // Overwrite TCP cksum + zero IPv4 cksum in the mbuf's data buffer.
    // Safety: the caller guarantees the data buffer holds at least
    // ETH_HDR_LEN + IPV4_HDR_MIN + TCP_HDR_MIN bytes populated by
    // build_segment / build_retrans_header. data_len reflects the
    // filled region, so the slice length is well-defined.
    let data_ptr = unsafe { dpdk_net_sys::shim_rte_pktmbuf_data(mbuf) } as *mut u8;
    let data_len = unsafe { dpdk_net_sys::shim_rte_pktmbuf_data_len(mbuf) } as usize;
    debug_assert!(
        !data_ptr.is_null(),
        "live TX mbuf must have a valid data pointer"
    );
    debug_assert!(
        data_len >= ETH_HDR_LEN + IPV4_HDR_MIN + TCP_HDR_MIN,
        "TX mbuf must have a full Eth+IPv4+TCP header populated before finalize"
    );
    let frame = unsafe { std::slice::from_raw_parts_mut(data_ptr, data_len) };
    let _ = tx_offload_rewrite_cksums(
        frame,
        seg.src_ip,
        seg.dst_ip,
        tcp_hdr_len,
        payload_for_csum_len,
    );
}

/// Feature-off variant. `hw-offload-tx-cksum` compiled out ⇒ the
/// finalizer is a no-op and the software full-fold checksums that
/// `build_segment` already wrote stay on the wire.
///
/// # Safety
/// No memory is read or written; `unsafe` only to match the feature-on
/// signature so TX call sites compile unchanged across feature configs.
#[cfg(not(feature = "hw-offload-tx-cksum"))]
pub unsafe fn tx_offload_finalize(
    _mbuf: *mut dpdk_net_sys::rte_mbuf,
    _seg: &SegmentTx,
    _payload_for_csum_len: u32,
    _offload_active: bool,
) {
    // No-op. Spec §6.4.
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::l3_ip::ip_decode;

    fn base() -> SegmentTx<'static> {
        SegmentTx {
            src_mac: [0x02, 0, 0, 0, 0, 1],
            dst_mac: [0x02, 0, 0, 0, 0, 2],
            src_ip: 0x0a_00_00_02,
            dst_ip: 0x0a_00_00_01,
            src_port: 40000,
            dst_port: 5000,
            seq: 0x1000,
            ack: 0,
            flags: TCP_SYN,
            window: 65535,
            options: crate::tcp_options::TcpOpts {
                mss: Some(1460),
                ..Default::default()
            },
            payload: &[],
        }
    }

    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn syn_frame_has_mss_option_and_valid_sizes() {
        let seg = base();
        let mut out = [0u8; 128];
        let n = build_segment(&seg, &mut out).unwrap();
        // 14 eth + 20 ip + 20 tcp + 4 mss = 58.
        assert_eq!(n, 58);
        // MSS option lives at offset 14+20+20 .. +4.
        assert_eq!(out[14 + 20 + 20], 2); // kind
        assert_eq!(out[14 + 20 + 21], 4); // len
        let mss = u16::from_be_bytes([out[14 + 20 + 22], out[14 + 20 + 23]]);
        assert_eq!(mss, 1460);
    }

    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn frame_ipv4_header_parses_roundtrip() {
        let seg = base();
        let mut out = [0u8; 128];
        let n = build_segment(&seg, &mut out).unwrap();
        let dec = ip_decode(&out[ETH_HDR_LEN..n], 0, false).expect("ip decode");
        assert_eq!(dec.protocol, IPPROTO_TCP);
        assert_eq!(dec.src_ip, 0x0a_00_00_02);
        assert_eq!(dec.dst_ip, 0x0a_00_00_01);
    }

    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn data_segment_with_payload_has_correct_tcp_csum() {
        let mut seg = base();
        let payload = b"HELLO";
        seg.flags = TCP_ACK | TCP_PSH;
        seg.options = crate::tcp_options::TcpOpts::default();
        seg.payload = payload;
        let mut out = [0u8; 128];
        let n = build_segment(&seg, &mut out).unwrap();
        // 14 + 20 + 20 + 5 = 59
        assert_eq!(n, 59);
        // Verify csum by recomputing: zero out the csum bytes and fold.
        let tcp_start = ETH_HDR_LEN + IPV4_HDR_MIN;
        let mut scratch = out[tcp_start..n].to_vec();
        scratch[16] = 0;
        scratch[17] = 0;
        let expected = tcp_checksum(seg.src_ip, seg.dst_ip, scratch.len() as u32, &scratch);
        let actual = u16::from_be_bytes([out[tcp_start + 16], out[tcp_start + 17]]);
        assert_eq!(expected, actual);
    }

    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn output_too_small_returns_none() {
        let seg = base();
        let mut out = [0u8; 50];
        assert!(build_segment(&seg, &mut out).is_none());
    }

    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn rst_frame_has_rst_flag_and_no_options() {
        let mut seg = base();
        seg.flags = TCP_RST | TCP_ACK;
        seg.options = crate::tcp_options::TcpOpts::default();
        let mut out = [0u8; 64];
        let n = build_segment(&seg, &mut out).unwrap();
        assert_eq!(n, 54);
        assert_eq!(out[14 + 20 + 13], TCP_RST | TCP_ACK);
    }

    // Task 9: retransmit primitive — the hdr-only builder must produce the
    // same L2/L3/TCP header bytes (including TCP checksum) as `build_segment`
    // would for the same SegmentTx if the payload were inline.
    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn build_retrans_header_matches_build_segment_header_prefix() {
        // Use an ACK+PSH data segment with a real payload. TS/SACK options
        // off keeps the header size aligned with the simplest case.
        let mut base = base();
        base.flags = TCP_ACK | TCP_PSH;
        base.options = crate::tcp_options::TcpOpts::default();
        let payload = b"hello-world";
        base.payload = payload;

        // Full segment with inline payload.
        let mut full_buf = [0u8; 128];
        let full_n = build_segment(&base, &mut full_buf).expect("build_segment");
        let hdr_len_expected = full_n - payload.len();

        // Retrans header: payload stripped from SegmentTx, payload_for_csum
        // carries the same bytes so the TCP checksum matches.
        let hdr_only_seg = SegmentTx {
            payload: &[],
            ..base
        };
        let mut hdr_buf = [0u8; 128];
        let hdr_n = build_retrans_header(&hdr_only_seg, payload, &mut hdr_buf)
            .expect("build_retrans_header");

        // Header length matches.
        assert_eq!(hdr_n, hdr_len_expected, "header length mismatch");
        // Byte-for-byte equality of the header region. This confirms:
        //   * IPv4 total-length field matches (uses declared payload len)
        //   * TCP checksum matches (pseudo-header payload len + payload
        //     bytes folded from `payload_for_csum`).
        assert_eq!(
            &full_buf[0..hdr_n],
            &hdr_buf[0..hdr_n],
            "header bytes should be identical across build_segment / build_retrans_header"
        );
    }

    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn build_retrans_header_with_options_matches_prefix() {
        // Same invariant with TS + SACK options present.
        let mut base = base();
        base.flags = TCP_ACK | TCP_PSH;
        base.options = crate::tcp_options::TcpOpts {
            timestamps: Some((0x1122_3344, 0xaabb_ccdd)),
            ..Default::default()
        };
        let payload = b"abcdefghij0123456789";
        base.payload = payload;

        let mut full_buf = [0u8; 160];
        let full_n = build_segment(&base, &mut full_buf).expect("build_segment");
        let hdr_len_expected = full_n - payload.len();

        let hdr_only_seg = SegmentTx {
            payload: &[],
            ..base
        };
        let mut hdr_buf = [0u8; 160];
        let hdr_n = build_retrans_header(&hdr_only_seg, payload, &mut hdr_buf)
            .expect("build_retrans_header");

        assert_eq!(hdr_n, hdr_len_expected);
        assert_eq!(&full_buf[0..hdr_n], &hdr_buf[0..hdr_n]);
    }

    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn build_retrans_header_returns_none_on_small_buf() {
        let seg = SegmentTx {
            payload: &[],
            ..base()
        };
        let mut out = [0u8; 30];
        assert!(build_retrans_header(&seg, b"payload", &mut out).is_none());
    }

    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn pseudo_header_only_cksum_matches_manual_fold() {
        use crate::l3_ip::internet_checksum;
        let src_ip: u32 = 0x0a000001;
        let dst_ip: u32 = 0x0a000002;
        let tcp_seg_len: u32 = 40;

        let mut pseudo = Vec::with_capacity(12);
        pseudo.extend_from_slice(&src_ip.to_be_bytes());
        pseudo.extend_from_slice(&dst_ip.to_be_bytes());
        pseudo.push(0);
        pseudo.push(crate::l3_ip::IPPROTO_TCP);
        pseudo.extend_from_slice(&(tcp_seg_len as u16).to_be_bytes());
        let manual = internet_checksum(&[&pseudo]);

        let helper = tcp_pseudo_header_checksum(src_ip, dst_ip, tcp_seg_len);
        assert_eq!(helper, manual,
            "tcp_pseudo_header_checksum must match manual fold of the 12-byte pseudo-header");
    }

    // A-HW Task 7: tx_offload_finalize exercises the memory-rewrite path
    // via the pure `tx_offload_rewrite_cksums` helper, which is testable
    // against a plain byte buffer without synthesizing an opaque rte_mbuf.
    // The ol_flags / l2-l4_len triple flows through sys-crate shims that
    // require a real mempool-allocated mbuf to call meaningfully; those
    // paths are covered by an integration smoke test under A-HW Task 13.
    //
    // The feature-off (offload_active=false) no-op path is covered by the
    // null-pointer short-circuit: the finalizer never dereferences when
    // offload_active is false, so a null mbuf pointer is safe.

    #[cfg(feature = "hw-offload-tx-cksum")]
    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn tx_offload_rewrite_cksums_writes_pseudo_and_zeroes_ip() {
        // Build a full segment with build_segment, then rewrite cksum
        // fields via the finalizer helper and verify bytes match the
        // expected pseudo-header-only TCP cksum + zero IPv4 cksum.
        let seg = base();
        let mut frame = [0u8; 128];
        let n = build_segment(&seg, &mut frame).expect("build_segment");
        let opts_len = seg.options.encoded_len();
        let tcp_hdr_len = TCP_HDR_MIN + opts_len;
        let payload_len = seg.payload.len() as u32;

        let ok = tx_offload_rewrite_cksums(
            &mut frame[..n],
            seg.src_ip,
            seg.dst_ip,
            tcp_hdr_len,
            payload_len,
        );
        assert!(ok);

        // IPv4 header cksum zeroed.
        let ip_cksum_off = ETH_HDR_LEN + 10;
        assert_eq!(frame[ip_cksum_off], 0);
        assert_eq!(frame[ip_cksum_off + 1], 0);

        // TCP cksum field == pseudo-header-only fold.
        let tcp_cksum_off = ETH_HDR_LEN + IPV4_HDR_MIN + 16;
        let pseudo_len = tcp_hdr_len as u32 + payload_len;
        let expected = tcp_pseudo_header_checksum(seg.src_ip, seg.dst_ip, pseudo_len);
        let actual = u16::from_be_bytes([frame[tcp_cksum_off], frame[tcp_cksum_off + 1]]);
        assert_eq!(actual, expected);
    }

    #[cfg(feature = "hw-offload-tx-cksum")]
    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn tx_offload_rewrite_cksums_rejects_short_frame() {
        let mut frame = [0u8; ETH_HDR_LEN + IPV4_HDR_MIN + TCP_HDR_MIN - 1];
        let ok = tx_offload_rewrite_cksums(&mut frame, 0x0a000001, 0x0a000002, 20, 0);
        assert!(!ok, "short frame must be rejected by rewrite helper");
    }

    #[cfg(feature = "hw-offload-tx-cksum")]
    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn tx_offload_finalize_noop_on_null_mbuf() {
        // offload_active == true but a null mbuf pointer hits the early
        // return. This also exercises the unsafe fn contract: caller may
        // pass null safely; nothing is dereferenced.
        let seg = base();
        unsafe {
            tx_offload_finalize(std::ptr::null_mut(), &seg, 128, true);
        }
    }

    #[cfg(feature = "hw-offload-tx-cksum")]
    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn tx_offload_finalize_noop_when_offload_inactive() {
        // offload_active == false ⇒ early return. With a null mbuf we
        // also verify the function does not dereference when inactive.
        let seg = base();
        unsafe {
            tx_offload_finalize(std::ptr::null_mut(), &seg, 128, false);
        }
    }

    // Feature-off build — the finalizer is a no-op stub. Exercise the
    // no-op signature to confirm it compiles + links across the feature
    // matrix. Runs only when hw-offload-tx-cksum is compile-off.
    #[cfg(not(feature = "hw-offload-tx-cksum"))]
    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn tx_offload_finalize_feature_off_is_noop() {
        let seg = base();
        unsafe {
            tx_offload_finalize(std::ptr::null_mut(), &seg, 128, true);
            tx_offload_finalize(std::ptr::null_mut(), &seg, 128, false);
        }
    }

    // PO4 wire-equivalence test: `build_segment_offload` MUST produce
    // the same final post-`tx_offload_rewrite_cksums` mbuf state as the
    // old `build_segment` + `tx_offload_rewrite_cksums` sequence, on
    // every offload-active TX. The mbuf bytes go to the NIC unchanged,
    // so byte-by-byte equality is the correctness gate.
    //
    // Why test both with payload and without: the payload path is the
    // common case (MSS-sized data segments); the empty-payload case
    // covers ACKs / RSTs / SYNs where the pseudo-header tcp_seg_len
    // collapses to just the TCP header length.
    #[cfg(feature = "hw-offload-tx-cksum")]
    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn build_segment_offload_wire_equivalent_to_build_segment_plus_rewrite() {
        // Data segment with a non-trivial payload (so the full fold has
        // real work, and the pseudo-only fold is provably different
        // before the rewrite).
        let mut seg = base();
        seg.flags = TCP_ACK | TCP_PSH;
        seg.options = crate::tcp_options::TcpOpts::default();
        let payload = b"PO4-wire-equivalence-fixture-bytes";
        seg.payload = payload;

        // Path A: build_segment (full fold) then tx_offload_rewrite_cksums.
        let mut a = [0u8; 256];
        let n_a = build_segment(&seg, &mut a).expect("build_segment");
        let opts_len = seg.options.encoded_len();
        let tcp_hdr_len = TCP_HDR_MIN + opts_len;
        let payload_len = seg.payload.len() as u32;
        let ok_a = tx_offload_rewrite_cksums(
            &mut a[..n_a],
            seg.src_ip,
            seg.dst_ip,
            tcp_hdr_len,
            payload_len,
        );
        assert!(ok_a);

        // Path B: build_segment_offload directly (no rewrite needed for
        // wire bytes, but production calls rewrite again via
        // tx_offload_finalize — that rewrite is idempotent for these
        // fields, so include it here too to mirror the call sequence).
        let mut b = [0u8; 256];
        let n_b = build_segment_offload(&seg, &mut b).expect("build_segment_offload");
        let ok_b = tx_offload_rewrite_cksums(
            &mut b[..n_b],
            seg.src_ip,
            seg.dst_ip,
            tcp_hdr_len,
            payload_len,
        );
        assert!(ok_b);

        // Same number of bytes, same bytes.
        assert_eq!(n_a, n_b, "frame length must match");
        assert_eq!(
            &a[..n_a],
            &b[..n_b],
            "wire bytes must be bit-identical after build_segment_offload + rewrite vs build_segment + rewrite"
        );

        // Spot-check the cksum fields explicitly: TCP cksum =
        // pseudo-only fold, IPv4 cksum = 0.
        let ip_cksum_off = ETH_HDR_LEN + 10;
        assert_eq!(b[ip_cksum_off], 0);
        assert_eq!(b[ip_cksum_off + 1], 0);
        let tcp_cksum_off = ETH_HDR_LEN + IPV4_HDR_MIN + 16;
        let pseudo_len = tcp_hdr_len as u32 + payload_len;
        let expected_tcp = tcp_pseudo_header_checksum(seg.src_ip, seg.dst_ip, pseudo_len);
        let actual_tcp = u16::from_be_bytes([b[tcp_cksum_off], b[tcp_cksum_off + 1]]);
        assert_eq!(actual_tcp, expected_tcp);
    }

    // Wire-equivalence on the empty-payload shape (ACK / SYN / RST). The
    // pseudo-only fold collapses to just (src_ip || dst_ip || 0 || proto
    // || tcp_hdr_len) but the test invariant is unchanged: same final
    // bytes after the rewrite. Catches off-by-one in the
    // pseudo-header-length argument or any divergence in the empty-
    // payload code path that escapes the data-segment test above.
    #[cfg(feature = "hw-offload-tx-cksum")]
    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn build_segment_offload_wire_equivalent_for_empty_payload() {
        let seg = base(); // SYN, no payload.

        let mut a = [0u8; 128];
        let n_a = build_segment(&seg, &mut a).expect("build_segment");
        let opts_len = seg.options.encoded_len();
        let tcp_hdr_len = TCP_HDR_MIN + opts_len;
        let payload_len = seg.payload.len() as u32;
        let ok_a = tx_offload_rewrite_cksums(
            &mut a[..n_a],
            seg.src_ip,
            seg.dst_ip,
            tcp_hdr_len,
            payload_len,
        );
        assert!(ok_a);

        let mut b = [0u8; 128];
        let n_b = build_segment_offload(&seg, &mut b).expect("build_segment_offload");
        let ok_b = tx_offload_rewrite_cksums(
            &mut b[..n_b],
            seg.src_ip,
            seg.dst_ip,
            tcp_hdr_len,
            payload_len,
        );
        assert!(ok_b);

        assert_eq!(n_a, n_b);
        assert_eq!(&a[..n_a], &b[..n_b]);
    }

    // Feature-off build of `build_segment_offload`: still produces the
    // same wire shape (pseudo-only TCP cksum + zero IPv4 cksum) — the
    // function is feature-agnostic (it does not call the gated
    // `tx_offload_rewrite_cksums`). Exercise the bytes-match invariant
    // by recomputing the expected fields manually.
    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn build_segment_offload_writes_pseudo_only_and_zero_ip_csum() {
        let mut seg = base();
        seg.flags = TCP_ACK | TCP_PSH;
        seg.options = crate::tcp_options::TcpOpts::default();
        let payload = b"hello";
        seg.payload = payload;

        let mut out = [0u8; 128];
        let n = build_segment_offload(&seg, &mut out).expect("build_segment_offload");

        // IPv4 cksum field = 0.
        let ip_cksum_off = ETH_HDR_LEN + 10;
        assert_eq!(out[ip_cksum_off], 0, "IPv4 cksum hi byte must be 0");
        assert_eq!(out[ip_cksum_off + 1], 0, "IPv4 cksum lo byte must be 0");

        // TCP cksum field = pseudo-header-only fold.
        let opts_len = seg.options.encoded_len();
        let tcp_hdr_len = TCP_HDR_MIN + opts_len;
        let pseudo_len = tcp_hdr_len as u32 + payload.len() as u32;
        let expected = tcp_pseudo_header_checksum(seg.src_ip, seg.dst_ip, pseudo_len);
        let tcp_cksum_off = ETH_HDR_LEN + IPV4_HDR_MIN + 16;
        let actual = u16::from_be_bytes([out[tcp_cksum_off], out[tcp_cksum_off + 1]]);
        assert_eq!(actual, expected, "TCP cksum must be pseudo-header-only fold");

        // Payload bytes still at the tail of the frame.
        assert_eq!(&out[n - payload.len()..n], &payload[..]);
    }

    // PO6 wire-equivalence test: `build_retrans_header_offload` must
    // produce the same final post-`tx_offload_rewrite_cksums` mbuf state
    // as the old `build_retrans_header(payload_bytes)` +
    // `tx_offload_rewrite_cksums` sequence, on every offload-active
    // retransmit. The mbuf bytes go to the NIC unchanged after chaining,
    // so byte-by-byte equality of the header region is the correctness
    // gate. PO6 is stronger than PO4: the offload variant takes only the
    // payload LENGTH, not the bytes, so the production caller skips the
    // `shim_rte_pktmbuf_data` FFI call that reads the chained data mbuf.
    #[cfg(feature = "hw-offload-tx-cksum")]
    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn build_retrans_header_offload_wire_equivalent_to_build_retrans_header_plus_rewrite() {
        // ACK+PSH retrans header with TS option present (production
        // retrans path always carries TS once TS is negotiated, per RFC
        // 7323 §3 MUST-22 — exercises the option encode path too).
        let mut base = base();
        base.flags = TCP_ACK | TCP_PSH;
        base.options = crate::tcp_options::TcpOpts {
            timestamps: Some((0x1122_3344, 0xaabb_ccdd)),
            ..Default::default()
        };
        let real_payload = b"PO6-wire-equivalence-fixture-bytes-with-some-length";
        // Important: hdr-only seg has empty payload (production retrans
        // path passes seg.payload == &[]). The payload bytes live in the
        // chained data mbuf.
        let hdr_only_seg = SegmentTx {
            payload: &[],
            ..base
        };
        let payload_len = real_payload.len() as u32;

        // Path A: build_retrans_header (full SW fold over real payload)
        // + tx_offload_rewrite_cksums.
        let mut a = [0u8; 160];
        let n_a = build_retrans_header(&hdr_only_seg, real_payload, &mut a)
            .expect("build_retrans_header");
        let opts_len = hdr_only_seg.options.encoded_len();
        let tcp_hdr_len = TCP_HDR_MIN + opts_len;
        let ok_a = tx_offload_rewrite_cksums(
            &mut a[..n_a],
            hdr_only_seg.src_ip,
            hdr_only_seg.dst_ip,
            tcp_hdr_len,
            payload_len,
        );
        assert!(ok_a);

        // Path B: build_retrans_header_offload (no SW fold, payload bytes
        // NOT read — only the length matters) + idempotent rewrite. The
        // production retransmit path calls the rewrite again via
        // tx_offload_finalize; including it here mirrors the call
        // sequence and exercises rewrite idempotency.
        let mut b = [0u8; 160];
        let n_b = build_retrans_header_offload(&hdr_only_seg, payload_len, &mut b)
            .expect("build_retrans_header_offload");
        let ok_b = tx_offload_rewrite_cksums(
            &mut b[..n_b],
            hdr_only_seg.src_ip,
            hdr_only_seg.dst_ip,
            tcp_hdr_len,
            payload_len,
        );
        assert!(ok_b);

        // Same number of bytes, same bytes.
        assert_eq!(n_a, n_b, "header length must match");
        assert_eq!(
            &a[..n_a],
            &b[..n_b],
            "wire header bytes must be bit-identical after build_retrans_header_offload + rewrite vs build_retrans_header + rewrite"
        );

        // Spot-check the cksum fields explicitly: TCP cksum =
        // pseudo-only fold (with declared payload_len in the
        // pseudo-header tcp_length field), IPv4 cksum = 0.
        let ip_cksum_off = ETH_HDR_LEN + 10;
        assert_eq!(b[ip_cksum_off], 0);
        assert_eq!(b[ip_cksum_off + 1], 0);
        let tcp_cksum_off = ETH_HDR_LEN + IPV4_HDR_MIN + 16;
        let pseudo_len = tcp_hdr_len as u32 + payload_len;
        let expected_tcp = tcp_pseudo_header_checksum(
            hdr_only_seg.src_ip,
            hdr_only_seg.dst_ip,
            pseudo_len,
        );
        let actual_tcp = u16::from_be_bytes([b[tcp_cksum_off], b[tcp_cksum_off + 1]]);
        assert_eq!(actual_tcp, expected_tcp);
    }

    // PO6: build_retrans_header_offload must reject framing lengths that
    // overflow the 16-bit IPv4 total-length field. Mirrors the existing
    // `build_retrans_header_returns_none_on_small_buf` shape but checks
    // the length-too-large guard rather than the buf-too-small one.
    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn build_retrans_header_offload_returns_none_on_oversize_payload_len() {
        let seg = SegmentTx {
            payload: &[],
            ..base()
        };
        let mut out = [0u8; 128];
        // u16::MAX + 1 — the IPv4 total-length field is 16 bits, so the
        // guard rejects a framing length that overflows it.
        let oversize = u16::MAX as u32 + 1;
        assert!(build_retrans_header_offload(&seg, oversize, &mut out).is_none());
    }

    // PO6: build_retrans_header_offload writes pseudo-only TCP cksum +
    // zero IPv4 cksum directly, without reading any payload bytes. This
    // is a feature-agnostic check (compiles + runs whether
    // hw-offload-tx-cksum is on or off — same as the
    // `build_segment_offload_writes_pseudo_only_*` sibling test).
    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn build_retrans_header_offload_writes_pseudo_only_and_zero_ip_csum() {
        let mut base = base();
        base.flags = TCP_ACK | TCP_PSH;
        base.options = crate::tcp_options::TcpOpts::default();
        let hdr_only_seg = SegmentTx {
            payload: &[],
            ..base
        };
        let payload_len: u32 = 1400; // typical MSS-sized retrans

        let mut out = [0u8; 128];
        let n = build_retrans_header_offload(&hdr_only_seg, payload_len, &mut out)
            .expect("build_retrans_header_offload");

        // No payload was written (retrans header only).
        let opts_len = hdr_only_seg.options.encoded_len();
        let tcp_hdr_len = TCP_HDR_MIN + opts_len;
        assert_eq!(n, ETH_HDR_LEN + IPV4_HDR_MIN + tcp_hdr_len);

        // IPv4 cksum field = 0.
        let ip_cksum_off = ETH_HDR_LEN + 10;
        assert_eq!(out[ip_cksum_off], 0, "IPv4 cksum hi byte must be 0");
        assert_eq!(out[ip_cksum_off + 1], 0, "IPv4 cksum lo byte must be 0");

        // IPv4 total-length reflects header + DECLARED payload length,
        // even though no payload bytes were written into `out`. This is
        // the on-wire framing field that the chained data mbuf will
        // satisfy.
        let ip_total_len = u16::from_be_bytes([out[ETH_HDR_LEN + 2], out[ETH_HDR_LEN + 3]]);
        assert_eq!(
            ip_total_len as u32,
            (IPV4_HDR_MIN + tcp_hdr_len) as u32 + payload_len
        );

        // TCP cksum field = pseudo-header-only fold over (src,dst,proto,
        // tcp_seg_len) where tcp_seg_len = tcp_hdr_len + payload_len.
        let tcp_cksum_off = ETH_HDR_LEN + IPV4_HDR_MIN + 16;
        let pseudo_len = tcp_hdr_len as u32 + payload_len;
        let expected = tcp_pseudo_header_checksum(
            hdr_only_seg.src_ip,
            hdr_only_seg.dst_ip,
            pseudo_len,
        );
        let actual = u16::from_be_bytes([out[tcp_cksum_off], out[tcp_cksum_off + 1]]);
        assert_eq!(actual, expected, "TCP cksum must be pseudo-header-only fold");
    }

    // build_segment (unchanged) MUST still produce a verifiable full-fold
    // TCP cksum on the offload-off path — this regression-guards the
    // CsumStrategy::Full branch after the inner refactor.
    #[cfg_attr(miri, ignore = "touches DPDK sys::*")]
    #[test]
    fn build_segment_full_fold_still_verifies_with_rfc_recompute() {
        let mut seg = base();
        seg.flags = TCP_ACK | TCP_PSH;
        seg.options = crate::tcp_options::TcpOpts::default();
        let payload = b"po4-regression-fixture";
        seg.payload = payload;

        let mut out = [0u8; 128];
        let n = build_segment(&seg, &mut out).expect("build_segment");
        let tcp_start = ETH_HDR_LEN + IPV4_HDR_MIN;
        let mut scratch = out[tcp_start..n].to_vec();
        scratch[16] = 0;
        scratch[17] = 0;
        let expected = tcp_checksum(seg.src_ip, seg.dst_ip, scratch.len() as u32, &scratch);
        let actual = u16::from_be_bytes([out[tcp_start + 16], out[tcp_start + 17]]);
        assert_eq!(expected, actual);
    }
}
