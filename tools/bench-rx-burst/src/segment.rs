//! Per-segment record + Readable-chunk header parser.
//!
//! Phase 8 of the 2026-05-09 bench-suite overhaul. The
//! `burst-echo-server` peer ships N back-to-back segments of W bytes
//! per BURST command; each segment carries a 16-byte header
//! `[be64 seq_idx | be64 peer_send_ns]`. The DUT's recv path may
//! coalesce several segments into one user-space delivery (engine
//! `Readable` event, kernel `read()`, F-Stack `ff_read()`); the parser
//! walks the bytes in W-byte steps and pulls one header per step.
//!
//! Cross-stack: dpdk_net + linux_kernel + fstack all produce the same
//! `Vec<SegmentRecord>` per bucket so the percentile + CSV emit logic
//! in `main.rs` is stack-agnostic.

/// One per-segment measurement. `latency_ns` is the DUT-side
/// `clock::now_ns() - peer_send_ns` (or `Instant::now()` delta for
/// linux_kernel) — skewed by NTP offset because `peer_send_ns` is
/// peer's `CLOCK_REALTIME`. See `burst-echo-server.c` header comment
/// for the trade-off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SegmentRecord {
    /// Bucket index (0-based) within the run's grid.
    pub bucket_id: u32,
    /// Burst index (0-based) within the bucket.
    pub burst_idx: u64,
    /// Segment index (0-based) within the burst — matches the
    /// `seq_idx` in the segment header.
    pub seg_idx: u64,
    /// Peer send timestamp in ns (CLOCK_REALTIME).
    pub peer_send_ns: u64,
    /// DUT recv timestamp in ns (`clock::now_ns()` for dpdk_net /
    /// fstack; deltas-from-burst-t0 for linux_kernel using
    /// `Instant::now()`). Same anchor convention as `peer_send_ns`
    /// where possible — for cross-host-comparable latency we expose
    /// the raw delta.
    pub dut_recv_ns: u64,
    /// `dut_recv_ns - peer_send_ns` (saturating). Negative deltas
    /// (clock skew) clamp to 0 so percentiles stay finite — the raw
    /// values are still recoverable from the CSV columns.
    pub latency_ns: u64,
}

impl SegmentRecord {
    /// Build a record from header bytes + DUT-side recv timestamp.
    /// Saturates `latency_ns` at 0 when `dut_recv_ns < peer_send_ns`
    /// (clock skew below NTP offset bound).
    pub fn new(
        bucket_id: u32,
        burst_idx: u64,
        seg_idx: u64,
        peer_send_ns: u64,
        dut_recv_ns: u64,
    ) -> Self {
        let latency_ns = dut_recv_ns.saturating_sub(peer_send_ns);
        Self {
            bucket_id,
            burst_idx,
            seg_idx,
            peer_send_ns,
            dut_recv_ns,
            latency_ns,
        }
    }
}

/// Parse all segment headers from a chunk that holds an integer
/// number of W-byte segments.
///
/// Returns `(seq_idx, peer_send_ns)` per segment. The chunk's length
/// MUST be a multiple of `W`; surplus bytes (a partial trailing
/// segment) are NOT parsed and the caller must keep them buffered for
/// the next chunk.
///
/// Header layout: bytes `[0..8]` = big-endian `u64` `seq_idx`,
/// bytes `[8..16]` = big-endian `u64` `peer_send_ns`. `W` MUST be
/// at least 16 (the header size); smaller `W` is malformed and is
/// rejected by the burst-echo-server upstream.
pub fn parse_burst_chunk(chunk: &[u8], w: usize) -> Vec<(u64, u64)> {
    if w < 16 {
        return Vec::new();
    }
    let n = chunk.len() / w;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let off = i * w;
        // Safety bounds: `chunk.len() >= n*w == (i+1)*w` so the
        // `[off..off+16]` slice is in-range.
        let seq_idx = u64::from_be_bytes(chunk[off..off + 8].try_into().unwrap());
        let peer_send_ns =
            u64::from_be_bytes(chunk[off + 8..off + 16].try_into().unwrap());
        out.push((seq_idx, peer_send_ns));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_burst_chunk_extracts_per_segment_headers() {
        // 3 segments of 32 bytes each, concatenated into one chunk.
        let mut chunk = Vec::new();
        for (idx, ts) in [(0u64, 1_000u64), (1, 2_000), (2, 3_000)] {
            chunk.extend_from_slice(&idx.to_be_bytes());
            chunk.extend_from_slice(&ts.to_be_bytes());
            chunk.extend_from_slice(&[0u8; 16]); // pad to W=32
        }

        let headers = parse_burst_chunk(&chunk, 32);
        assert_eq!(headers.len(), 3);
        assert_eq!(headers[0], (0, 1_000));
        assert_eq!(headers[1], (1, 2_000));
        assert_eq!(headers[2], (2, 3_000));
    }

    /// Coalesced delivery (16 segments × 64 B = 1024 B in one chunk)
    /// is the canonical small-burst case — the DUT's stack may coalesce
    /// several segments into one Readable / read() / ff_read() event
    /// when MSS allows it. Parser must walk all 16 headers.
    #[test]
    fn parse_burst_chunk_handles_coalesced_small_segments() {
        let n = 16usize;
        let w = 64usize;
        let mut chunk = Vec::with_capacity(n * w);
        for i in 0..n {
            chunk.extend_from_slice(&(i as u64).to_be_bytes());
            chunk.extend_from_slice(&((i as u64) * 1_000).to_be_bytes());
            chunk.extend_from_slice(&vec![0u8; w - 16]);
        }
        let headers = parse_burst_chunk(&chunk, w);
        assert_eq!(headers.len(), n);
        for i in 0..n {
            assert_eq!(headers[i], (i as u64, (i as u64) * 1_000));
        }
    }

    /// Partial chunks (chunk.len() % w != 0) parse the integer-number-of-W
    /// prefix and ignore the trailing partial. Caller is responsible for
    /// re-buffering the partial across chunks.
    #[test]
    fn parse_burst_chunk_ignores_trailing_partial() {
        let mut chunk = Vec::new();
        // Segment 0 at offset 0.
        chunk.extend_from_slice(&0u64.to_be_bytes());
        chunk.extend_from_slice(&111u64.to_be_bytes());
        chunk.extend_from_slice(&[0u8; 48]); // pad to W=64
        // Trailing partial bytes (32B of a second segment).
        chunk.extend_from_slice(&[0xAA; 32]);
        let headers = parse_burst_chunk(&chunk, 64);
        assert_eq!(headers.len(), 1);
        assert_eq!(headers[0], (0, 111));
    }

    /// Reject W < 16 (header doesn't fit). The peer's burst-echo-server
    /// rejects this upstream; the parser still degrades cleanly to an
    /// empty Vec rather than panicking on a malformed slice.
    #[test]
    fn parse_burst_chunk_rejects_too_small_w() {
        let chunk = vec![0u8; 32];
        assert!(parse_burst_chunk(&chunk, 8).is_empty());
        assert!(parse_burst_chunk(&chunk, 15).is_empty());
    }

    /// Empty chunk -> empty Vec.
    #[test]
    fn parse_burst_chunk_empty() {
        let chunk: [u8; 0] = [];
        assert!(parse_burst_chunk(&chunk, 64).is_empty());
    }

    #[test]
    fn segment_record_saturating_latency() {
        // peer_send_ns > dut_recv_ns (NTP-offset case) → clamps to 0.
        let r = SegmentRecord::new(0, 0, 0, 1_000_000, 0);
        assert_eq!(r.latency_ns, 0);
        assert_eq!(r.peer_send_ns, 1_000_000);
        assert_eq!(r.dut_recv_ns, 0);

        let r = SegmentRecord::new(0, 0, 0, 1_000, 5_000);
        assert_eq!(r.latency_ns, 4_000);
    }
}
