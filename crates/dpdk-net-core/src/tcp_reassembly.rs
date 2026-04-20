//! Out-of-order segment reassembly. Spec §7.2 envisions mbuf-chain
//! zero-copy storage; we continue A3's AD-7 copy-based model by holding
//! `Vec<u8>` payloads per OOO segment. See AD-A4-reassembly in the plan
//! header.
//!
//! Insertion is O(N) where N is the number of OOO segments currently
//! buffered (bounded by `recv_buffer_bytes / peer_mss`, typically < 180
//! with a 256 KiB cap and 1460-byte MSS — acceptable under trading
//! workload where OOO is rare to begin with). Merge on insert uses
//! mTCP-style `CanMerge` / `MergeFragments` semantics applied to
//! copy-based `(seq, Vec<u8>)` entries.

use resd_net_sys as sys;

use crate::tcp_seq::{seq_le, seq_lt};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OooSegment {
    Bytes(OooBytes),
    MbufRef(OooMbufRef),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OooBytes {
    pub seq: u32,
    pub payload: Vec<u8>,
}

/// A6.5 Task 4b placeholder: references a segment of payload bytes
/// held inside a DPDK mbuf. Unreachable during Task 6 (4a); insert
/// path starts producing this variant in Task 7 (4b).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OooMbufRef {
    pub seq: u32,
    pub mbuf: std::ptr::NonNull<sys::rte_mbuf>,
    pub offset: u16,
    pub len: u16,
}

// MbufRef's raw pointer is not Send/Sync. ReorderQueue is single-
// lcore, so we add the marker impls to satisfy any container
// bounds downstream.
unsafe impl Send for OooMbufRef {}

impl OooSegment {
    pub fn seq(&self) -> u32 {
        match self {
            OooSegment::Bytes(b) => b.seq,
            OooSegment::MbufRef(m) => m.seq,
        }
    }

    pub fn end_seq(&self) -> u32 {
        match self {
            OooSegment::Bytes(b) => b.seq.wrapping_add(b.payload.len() as u32),
            OooSegment::MbufRef(m) => m.seq.wrapping_add(m.len as u32),
        }
    }

    pub fn len(&self) -> u32 {
        match self {
            OooSegment::Bytes(b) => b.payload.len() as u32,
            OooSegment::MbufRef(m) => m.len as u32,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Outcome of `ReorderQueue::insert`. The caller uses it to decide
/// whether to bump `tcp.rx_reassembly_queued` (true when new bytes
/// were actually buffered) and how many bytes were dropped due to
/// the cap (feeds `tcp.recv_buf_drops`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InsertOutcome {
    /// Bytes of new (not-already-buffered) payload actually stored.
    pub newly_buffered: u32,
    /// Bytes dropped because inserting them would push
    /// `total_bytes()` past `cap`.
    pub cap_dropped: u32,
}

pub struct ReorderQueue {
    segments: Vec<OooSegment>,
    cap: u32,
    total_bytes: u32,
}

impl ReorderQueue {
    pub fn new(cap: u32) -> Self {
        Self {
            segments: Vec::new(),
            cap,
            total_bytes: 0,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.segments.is_empty()
    }
    pub fn len(&self) -> usize {
        self.segments.len()
    }
    pub fn total_bytes(&self) -> u32 {
        self.total_bytes
    }
    pub fn segments(&self) -> &[OooSegment] {
        &self.segments
    }

    /// Insert a new OOO segment. Merges with neighbours where ranges
    /// overlap or touch; drops payload past `cap`. Returns an outcome
    /// summary that the caller feeds into counters.
    pub fn insert(&mut self, seq: u32, payload: &[u8]) -> InsertOutcome {
        if payload.is_empty() {
            return InsertOutcome {
                newly_buffered: 0,
                cap_dropped: 0,
            };
        }
        let incoming_end = seq.wrapping_add(payload.len() as u32);

        // Carve the incoming payload into gap-slices that don't overlap
        // any existing segment. Each gap-slice is then inserted and
        // merged with its neighbours.
        let mut cursor = seq;
        let mut newly_buffered = 0u32;
        let mut cap_dropped = 0u32;
        let mut to_insert: Vec<(u32, Vec<u8>)> = Vec::new();

        for existing in &self.segments {
            if seq_le(incoming_end, existing.seq()) {
                break;
            }
            if seq_le(existing.end_seq(), cursor) {
                continue;
            }
            if seq_lt(cursor, existing.seq()) {
                let gap_len = existing.seq().wrapping_sub(cursor) as usize;
                let off = cursor.wrapping_sub(seq) as usize;
                let take_end = off + gap_len.min(payload.len() - off);
                let remaining_cap = self.cap.saturating_sub(self.total_bytes + newly_buffered);
                let take = (take_end - off).min(remaining_cap as usize);
                if take > 0 {
                    to_insert.push((cursor, payload[off..off + take].to_vec()));
                    newly_buffered += take as u32;
                }
                if take < take_end - off {
                    cap_dropped += (take_end - off - take) as u32;
                }
                cursor = cursor.wrapping_add((take_end - off) as u32);
            }
            if seq_lt(cursor, existing.end_seq()) {
                cursor = existing.end_seq();
            }
        }

        if seq_lt(cursor, incoming_end) {
            let off = cursor.wrapping_sub(seq) as usize;
            let tail_len = payload.len() - off;
            let remaining_cap = self.cap.saturating_sub(self.total_bytes + newly_buffered);
            let take = tail_len.min(remaining_cap as usize);
            if take > 0 {
                to_insert.push((cursor, payload[off..off + take].to_vec()));
                newly_buffered += take as u32;
            }
            if take < tail_len {
                cap_dropped += (tail_len - take) as u32;
            }
        }

        for (s, p) in to_insert {
            self.insert_merged(s, p);
        }

        self.total_bytes += newly_buffered;
        InsertOutcome {
            newly_buffered,
            cap_dropped,
        }
    }

    /// Insert `(seq, payload)` which is guaranteed not to overlap any
    /// existing segment. Merges on touch (adjacent ranges coalesce).
    fn insert_merged(&mut self, seq: u32, payload: Vec<u8>) {
        let end = seq.wrapping_add(payload.len() as u32);

        let mut idx = self.segments.len();
        for (i, s) in self.segments.iter().enumerate() {
            if seq_lt(seq, s.seq()) {
                idx = i;
                break;
            }
        }

        let mut merged_left = false;
        if idx > 0 && self.segments[idx - 1].end_seq() == seq {
            match &mut self.segments[idx - 1] {
                OooSegment::Bytes(b) => {
                    b.payload.extend_from_slice(&payload);
                    merged_left = true;
                }
                OooSegment::MbufRef(_) => {
                    // Cross-variant merge not supported; fall through to insert.
                    // Task 4a's MbufRef variant is unreachable so this arm is dead
                    // until Task 7. Guarded here to avoid a cross-variant merge
                    // bug when 4b lands.
                }
            }
        }

        if idx < self.segments.len() && self.segments[idx].seq() == end {
            if merged_left {
                // Merge right into (idx-1). Only works Bytes+Bytes.
                let right = self.segments.remove(idx);
                match (&mut self.segments[idx - 1], right) {
                    (OooSegment::Bytes(left_b), OooSegment::Bytes(right_b)) => {
                        left_b.payload.extend_from_slice(&right_b.payload);
                    }
                    (_, right) => {
                        // Cross-variant: restore right and skip merge.
                        self.segments.insert(idx, right);
                    }
                }
            } else {
                match &mut self.segments[idx] {
                    OooSegment::Bytes(right_b) => {
                        let mut new_payload = payload;
                        new_payload.extend_from_slice(&right_b.payload);
                        right_b.seq = seq;
                        right_b.payload = new_payload;
                    }
                    OooSegment::MbufRef(_) => {
                        // Cross-variant: insert as new Bytes entry.
                        self.segments
                            .insert(idx, OooSegment::Bytes(OooBytes { seq, payload }));
                    }
                }
            }
        } else if !merged_left {
            self.segments
                .insert(idx, OooSegment::Bytes(OooBytes { seq, payload }));
        }
    }

    /// Pop the contiguous prefix of segments whose seq range starts at
    /// or before `rcv_nxt`. For each popped segment, yield the portion
    /// of its payload that lies at or after `rcv_nxt`. Returns the
    /// concatenated bytes and the number of segments drained.
    pub fn drain_contiguous_from(&mut self, mut rcv_nxt: u32) -> (Vec<u8>, u32) {
        let mut out = Vec::new();
        let mut drained_segments = 0u32;

        while !self.segments.is_empty() {
            let seg_seq = self.segments[0].seq();
            if seq_lt(rcv_nxt, seg_seq) {
                break;
            }
            let seg_end = self.segments[0].end_seq();
            let seg_len = self.segments[0].len();
            if seq_le(seg_end, rcv_nxt) {
                // Entire segment behind rcv_nxt — drop.
                self.total_bytes = self.total_bytes.saturating_sub(seg_len);
                self.segments.remove(0);
                drained_segments += 1;
                continue;
            }
            let skip = rcv_nxt.wrapping_sub(seg_seq) as usize;
            match &self.segments[0] {
                OooSegment::Bytes(b) => out.extend_from_slice(&b.payload[skip..]),
                OooSegment::MbufRef(m) => {
                    // Task 4a: Bytes variant is the only one currently produced;
                    // MbufRef cannot be reached here until Task 4b flips the
                    // insert path. Panic surfaces any premature MbufRef
                    // reachability.
                    unreachable!(
                        "OOO drain reached MbufRef at {:?} before Task 4b insert path is wired",
                        m
                    );
                }
            }
            rcv_nxt = seg_end;
            self.total_bytes = self.total_bytes.saturating_sub(seg_len);
            self.segments.remove(0);
            drained_segments += 1;
        }
        (out, drained_segments)
    }
}

/// Test helper: project an `OooSegment` back to its `OooBytes`
/// variant, panicking if it's the MbufRef variant. Only the Bytes
/// variant is reachable in Task 6 (4a); Task 7 (4b) flips the
/// insert path.
#[cfg(test)]
pub(crate) fn expect_bytes(seg: &OooSegment) -> &OooBytes {
    match seg {
        OooSegment::Bytes(b) => b,
        _ => panic!("expected Bytes variant, got {:?}", seg),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_queue_is_empty() {
        let q = ReorderQueue::new(1024);
        assert!(q.is_empty());
        assert_eq!(q.total_bytes(), 0);
    }

    #[test]
    fn single_insert_buffers_payload() {
        let mut q = ReorderQueue::new(1024);
        let out = q.insert(100, b"abcde");
        assert_eq!(out.newly_buffered, 5);
        assert_eq!(out.cap_dropped, 0);
        assert_eq!(q.len(), 1);
        assert_eq!(q.segments()[0].seq(), 100);
        assert_eq!(&expect_bytes(&q.segments()[0]).payload, b"abcde");
    }

    #[test]
    fn two_disjoint_inserts_stay_separate() {
        let mut q = ReorderQueue::new(1024);
        q.insert(100, b"aaa");
        q.insert(200, b"bbb");
        assert_eq!(q.len(), 2);
        assert_eq!(q.total_bytes(), 6);
    }

    #[test]
    fn inserts_sort_by_seq_even_if_arrival_order_reverses() {
        let mut q = ReorderQueue::new(1024);
        q.insert(200, b"bbb");
        q.insert(100, b"aaa");
        assert_eq!(q.segments()[0].seq(), 100);
        assert_eq!(q.segments()[1].seq(), 200);
    }

    #[test]
    fn adjacent_inserts_merge_into_one() {
        let mut q = ReorderQueue::new(1024);
        q.insert(100, b"abc");
        q.insert(103, b"def");
        assert_eq!(q.len(), 1);
        assert_eq!(q.segments()[0].seq(), 100);
        assert_eq!(&expect_bytes(&q.segments()[0]).payload, b"abcdef");
    }

    #[test]
    fn adjacent_insert_collapses_both_sides() {
        let mut q = ReorderQueue::new(1024);
        q.insert(100, b"aaa");
        q.insert(106, b"ccc");
        q.insert(103, b"bbb");
        assert_eq!(q.len(), 1);
        assert_eq!(q.segments()[0].seq(), 100);
        assert_eq!(&expect_bytes(&q.segments()[0]).payload, b"aaabbbccc");
    }

    #[test]
    fn overlap_with_existing_is_deduplicated() {
        let mut q = ReorderQueue::new(1024);
        q.insert(100, b"abcdef");
        // Existing range: [100..106). Retransmit: [103..107).
        // Overlap: [103..106). New: [106..107) — one byte.
        let out = q.insert(103, b"defg");
        assert_eq!(out.newly_buffered, 1);
        assert_eq!(out.cap_dropped, 0);
        assert_eq!(q.len(), 1);
        assert_eq!(&expect_bytes(&q.segments()[0]).payload, b"abcdefg");
    }

    #[test]
    fn cap_truncates_excess_and_reports_drop() {
        let mut q = ReorderQueue::new(4);
        let out = q.insert(100, b"abcdef");
        assert_eq!(out.newly_buffered, 4);
        assert_eq!(out.cap_dropped, 2);
        assert_eq!(&expect_bytes(&q.segments()[0]).payload, b"abcd");
    }

    #[test]
    fn empty_payload_insert_is_noop() {
        let mut q = ReorderQueue::new(1024);
        let out = q.insert(100, b"");
        assert_eq!(out.newly_buffered, 0);
        assert!(q.is_empty());
    }

    #[test]
    fn drain_with_no_contiguous_front_returns_empty() {
        let mut q = ReorderQueue::new(1024);
        q.insert(200, b"zzz");
        let (bytes, n) = q.drain_contiguous_from(100);
        assert!(bytes.is_empty());
        assert_eq!(n, 0);
        assert_eq!(q.len(), 1);
    }

    #[test]
    fn drain_single_adjacent_segment() {
        let mut q = ReorderQueue::new(1024);
        q.insert(100, b"abc");
        let (bytes, n) = q.drain_contiguous_from(100);
        assert_eq!(&bytes, b"abc");
        assert_eq!(n, 1);
        assert!(q.is_empty());
        assert_eq!(q.total_bytes(), 0);
    }

    #[test]
    fn drain_chains_through_touching_segments() {
        let mut q = ReorderQueue::new(1024);
        q.insert(100, b"aaa");
        q.insert(103, b"bbb");
        q.insert(200, b"zzz");
        let (bytes, n) = q.drain_contiguous_from(100);
        assert_eq!(&bytes, b"aaabbb");
        assert_eq!(n, 1);
        assert_eq!(q.len(), 1);
        assert_eq!(q.segments()[0].seq(), 200);
    }

    #[test]
    fn drain_with_rcv_nxt_inside_segment_skips_prefix() {
        let mut q = ReorderQueue::new(1024);
        q.insert(100, b"abcdef");
        let (bytes, n) = q.drain_contiguous_from(103);
        assert_eq!(&bytes, b"def");
        assert_eq!(n, 1);
    }

    #[test]
    fn drain_past_end_of_segment_drops_entirely() {
        let mut q = ReorderQueue::new(1024);
        q.insert(100, b"abc");
        let (bytes, n) = q.drain_contiguous_from(200);
        assert!(bytes.is_empty());
        assert_eq!(n, 1);
        assert!(q.is_empty());
    }

    #[test]
    fn drain_empty_queue_is_noop() {
        let mut q = ReorderQueue::new(1024);
        let (bytes, n) = q.drain_contiguous_from(500);
        assert!(bytes.is_empty());
        assert_eq!(n, 0);
    }
}
