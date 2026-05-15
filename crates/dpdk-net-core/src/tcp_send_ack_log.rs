//! Per-segment send→ACK latency ringbuffer.
//!
//! Records `(seq_range, t_send_ns)` on every TCP segment emit. On every
//! cumulative-ACK delivered to the conn, walks the ringbuffer head and
//! emits one `SendAckSample` per segment fully covered by the new
//! `snd_una`. Partial coverage is left for the next ACK.
//!
//! ## Disabled by default
//!
//! Recording is hot-path. The default `Conn` carries a zero-capacity
//! log so `record_send` early-returns. Bench-mode initialisation
//! (`Engine::enable_send_ack_logging(cap)`) replaces the per-conn log
//! with one of `cap` slots so future records actually retain the
//! samples. The runtime branch in `record_send` is:
//!
//! ```text
//! if self.cap == 0 { return; }
//! ```
//!
//! …a single predictable conditional the branch predictor consistently
//! takes, so the disabled-path overhead is one not-taken jump per emit.
//!
//! ## Retransmit handling
//!
//! On retransmit of a segment whose `[begin, end)` already lives in the
//! queue, `record_send` updates the existing entry's `t_send_ns` rather
//! than pushing a duplicate. Linear scan is O(n) on the queue; for the
//! bench-mode `cap=4096` ceiling this is a few-µs slow-path cost paid
//! only when retransmission actually happens (rare in healthy flows).
//! The latency sample on cum-ACK is then the (re-)send→ACK time of the
//! most recent emission of that range — the natural choice for
//! retransmit-aware send→ACK latency. Perfect retransmit-vs-original
//! attribution is a Phase 11+ concern.
//!
//! ## Scope
//!
//! This module is bench-side instrumentation only. It does not feed
//! into RTT estimation, RACK, RTO, or any production decision-making
//! path. The samples are drained by bench tooling and do not influence
//! TCP stack behavior.

use std::collections::VecDeque;

#[derive(Copy, Clone, Debug)]
pub struct SeqRange {
    pub begin: u32,
    pub end: u32,
}

#[derive(Copy, Clone, Debug)]
pub struct SendAckSample {
    pub begin_seq: u32,
    pub end_seq: u32,
    pub latency_ns: u64,
}

pub struct SendAckLog {
    entries: VecDeque<(SeqRange, u64)>,
    cap: usize,
    pub overflow_drops: u64,
}

impl SendAckLog {
    /// Disabled — `record_send` is a no-op. Used as the default per-conn
    /// log so the disabled-path branch is `cap == 0` early-return.
    pub fn disabled() -> Self {
        Self {
            entries: VecDeque::new(),
            cap: 0,
            overflow_drops: 0,
        }
    }

    pub fn with_capacity(cap: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(cap),
            cap,
            overflow_drops: 0,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.cap > 0
    }

    /// Enable an existing `disabled()` log by giving it a capacity.
    /// No-op if already enabled with the same capacity. If the existing
    /// cap differs, the entries Vec is rebuilt (drops any pending records
    /// — bench mode is single-threaded by the engine's lcore so the
    /// transition is harmless).
    pub fn set_capacity(&mut self, cap: usize) {
        if cap == 0 {
            // Disabling — drop all entries.
            self.entries.clear();
            self.cap = 0;
            return;
        }
        if self.cap == cap {
            return;
        }
        self.entries = VecDeque::with_capacity(cap);
        self.cap = cap;
        self.overflow_drops = 0;
    }

    /// Record a segment emission. No-op when disabled (cap == 0).
    /// On retransmit (existing entry with same `begin`), updates the
    /// stored `t_send_ns` rather than pushing a duplicate.
    pub fn record_send(&mut self, range: SeqRange, t_send_ns: u64) {
        if self.cap == 0 {
            return;
        }
        // Retransmit detection: linear scan for an existing entry whose
        // `begin` matches. O(n) — acceptable for `cap=4096` and only paid
        // on retransmit (rare). Covers the case where send_bytes records
        // [seq, seq+len) and a later RTO/RACK/TLP retransmit re-emits the
        // same range; the sample on ACK should reflect the most recent
        // (re-)send→ACK time, not the original.
        for entry in self.entries.iter_mut() {
            if entry.0.begin == range.begin && entry.0.end == range.end {
                entry.1 = t_send_ns;
                return;
            }
        }
        if self.entries.len() == self.cap {
            self.entries.pop_front();
            self.overflow_drops += 1;
        }
        self.entries.push_back((range, t_send_ns));
    }

    /// Walk the ringbuffer head and pop every entry fully covered by
    /// `snd_una`. Returns one `SendAckSample` per popped entry.
    /// No-op when disabled.
    pub fn observe_cumulative_ack(&mut self, snd_una: u32, t_ack_ns: u64) -> Vec<SendAckSample> {
        let mut out = Vec::new();
        if self.cap == 0 {
            return out;
        }
        while let Some((range, t_send)) = self.entries.front().copied() {
            if seq_le(range.end, snd_una) {
                out.push(SendAckSample {
                    begin_seq: range.begin,
                    end_seq: range.end,
                    latency_ns: t_ack_ns.saturating_sub(t_send),
                });
                self.entries.pop_front();
            } else {
                break;
            }
        }
        out
    }
}

/// `a <= b` in TCP-sequence-space (32-bit wrap aware).
fn seq_le(a: u32, b: u32) -> bool {
    (b.wrapping_sub(a) as i32) >= 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_send_then_match_cumulative_ack_returns_latency() {
        let mut log = SendAckLog::with_capacity(16);
        log.record_send(SeqRange { begin: 100, end: 200 }, 1_000);
        log.record_send(SeqRange { begin: 200, end: 300 }, 1_500);
        log.record_send(SeqRange { begin: 300, end: 400 }, 2_000);

        // Cumulative ACK 250 covers first segment; second is partial (200..300, snd_una=250).
        // We attribute first segment fully (range.end=200 <= 250). Second stays queued.
        let acks = log.observe_cumulative_ack(250, 3_000);
        assert_eq!(acks.len(), 1);
        assert_eq!(acks[0].begin_seq, 100);
        assert_eq!(acks[0].end_seq, 200);
        assert_eq!(acks[0].latency_ns, 2_000);

        // Cumulative ACK 400 covers segments 2 and 3.
        let acks2 = log.observe_cumulative_ack(400, 4_000);
        assert_eq!(acks2.len(), 2);
        assert_eq!(acks2[0].latency_ns, 2_500);
        assert_eq!(acks2[1].latency_ns, 2_000);
    }

    #[test]
    fn capacity_overflow_drops_oldest_and_increments_counter() {
        let mut log = SendAckLog::with_capacity(2);
        log.record_send(SeqRange { begin: 100, end: 200 }, 1_000);
        log.record_send(SeqRange { begin: 200, end: 300 }, 2_000);
        log.record_send(SeqRange { begin: 300, end: 400 }, 3_000); // pushes out [100,200)

        assert_eq!(log.overflow_drops, 1);
        let acks = log.observe_cumulative_ack(400, 4_000);
        // First segment dropped; only segments 2 and 3 emit samples.
        assert_eq!(acks.len(), 2);
        assert_eq!(acks[0].begin_seq, 200);
    }

    #[test]
    fn handles_seq_wrap() {
        let mut log = SendAckLog::with_capacity(4);
        let near_max = u32::MAX - 100;
        log.record_send(
            SeqRange {
                begin: near_max,
                end: near_max.wrapping_add(150),
            },
            1_000,
        );

        // ACK that wraps past u32::MAX
        let new_snd_una = near_max.wrapping_add(150);
        let acks = log.observe_cumulative_ack(new_snd_una, 2_000);
        assert_eq!(acks.len(), 1);
        assert_eq!(acks[0].latency_ns, 1_000);
    }

    #[test]
    fn empty_log_observe_returns_no_samples() {
        let mut log = SendAckLog::with_capacity(4);
        let acks = log.observe_cumulative_ack(100, 1_000);
        assert!(acks.is_empty());
    }

    #[test]
    fn ack_below_oldest_seg_emits_nothing() {
        let mut log = SendAckLog::with_capacity(4);
        log.record_send(SeqRange { begin: 200, end: 300 }, 1_000);
        // ACK 150 (somehow before the segment) — must NOT emit.
        let acks = log.observe_cumulative_ack(150, 2_000);
        assert!(acks.is_empty());
    }

    #[test]
    fn disabled_log_is_noop_for_record_and_observe() {
        let mut log = SendAckLog::disabled();
        assert!(!log.is_enabled());
        log.record_send(SeqRange { begin: 100, end: 200 }, 1_000);
        let acks = log.observe_cumulative_ack(200, 2_000);
        assert!(acks.is_empty());
        assert_eq!(log.overflow_drops, 0);
    }

    #[test]
    fn retransmit_updates_existing_entry_and_uses_latest_t_send() {
        let mut log = SendAckLog::with_capacity(8);
        // Original send.
        log.record_send(SeqRange { begin: 1000, end: 1500 }, 10_000);
        log.record_send(SeqRange { begin: 1500, end: 2000 }, 11_000);
        // Retransmit of [1000,1500) at a later time.
        log.record_send(SeqRange { begin: 1000, end: 1500 }, 50_000);
        // Queue length unchanged (retransmit updated, not pushed).
        // No way to read entries.len() externally; verify via overflow stays 0
        // (would have been 1 if cap=2 and a duplicate was pushed).
        assert_eq!(log.overflow_drops, 0);

        // Cum ACK covering both segments.
        let acks = log.observe_cumulative_ack(2000, 60_000);
        assert_eq!(acks.len(), 2);
        // First entry is [1000,1500) with latest t_send = 50_000 → latency 10_000.
        assert_eq!(acks[0].begin_seq, 1000);
        assert_eq!(acks[0].latency_ns, 10_000);
        // Second entry is [1500,2000) with t_send = 11_000 → latency 49_000.
        assert_eq!(acks[1].begin_seq, 1500);
        assert_eq!(acks[1].latency_ns, 49_000);
    }
}
