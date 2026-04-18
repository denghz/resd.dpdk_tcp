//! RFC 8985 RACK state + loss detection.
//! Consumed by the Task 15 RACK detect-lost pass in `tcp_input.rs`.

#[derive(Debug, Clone, Default)]
pub struct RackState {
    /// RFC 8985 §6.1 RACK.xmit_ts — latest transmit timestamp of any
    /// acknowledged segment.
    pub xmit_ts_ns: u64,
    /// RFC 8985 §6.1 RACK.end_seq — end-seq of the segment with xmit_ts.
    pub end_seq: u32,
    /// Current reordering window (µs). Updated per-ACK via compute_reo_wnd_us.
    pub reo_wnd_us: u32,
    /// Minimum RTT observed on this conn. 0 until the first sample.
    pub min_rtt_us: u32,
    /// Whether we've observed a DSACK from the peer (RFC 2883). A5 tracks
    /// this for visibility (tcp.rx_dsack); Stage 2 may use it for reo_wnd
    /// adaptation per RFC 8985 §7.
    pub dsack_seen: bool,
}

impl RackState {
    pub fn new() -> Self {
        Self::default()
    }

    /// RFC 8985 §6.1: update RACK.xmit_ts / RACK.end_seq when an ACK newly
    /// covers `entry` such that entry's xmit is later than the current
    /// RACK, OR equal with higher end_seq.
    pub fn update_on_ack(&mut self, entry_xmit_ts_ns: u64, entry_end_seq: u32) {
        if entry_xmit_ts_ns > self.xmit_ts_ns
            || (entry_xmit_ts_ns == self.xmit_ts_ns
                && crate::tcp_seq::seq_lt(self.end_seq, entry_end_seq))
        {
            self.xmit_ts_ns = entry_xmit_ts_ns;
            self.end_seq = entry_end_seq;
        }
    }

    /// Track min RTT seen on this conn. RFC 8985 §6.2 uses this as half
    /// of the reo_wnd upper bound.
    pub fn update_min_rtt(&mut self, rtt_us: u32) {
        if self.min_rtt_us == 0 || rtt_us < self.min_rtt_us {
            self.min_rtt_us = rtt_us;
        }
    }

    /// RFC 8985 §6.2 detect-lost rule for `entry`.
    /// Returns true iff:
    /// - entry.xmit_ts < RACK.xmit_ts (or equal with lower end_seq), AND
    /// - now - entry.xmit_ts > reo_wnd_us.
    pub fn detect_lost(
        &self,
        entry_xmit_ts_ns: u64,
        entry_end_seq: u32,
        now_ns: u64,
        reo_wnd_us: u32,
    ) -> bool {
        let newer_ack_exists = entry_xmit_ts_ns < self.xmit_ts_ns
            || (entry_xmit_ts_ns == self.xmit_ts_ns
                && crate::tcp_seq::seq_lt(entry_end_seq, self.end_seq));
        if !newer_ack_exists {
            return false;
        }
        let age_ns = now_ns.saturating_sub(entry_xmit_ts_ns);
        age_ns > (reo_wnd_us as u64) * 1_000
    }
}

/// Compute reo_wnd per RFC 8985 §6.2.
/// - When `rack_aggressive` is true, returns 0 (per-connect opt).
/// - Otherwise min(SRTT/4, min_rtt/2), floored at 1000µs (1ms) to avoid
///   spurious loss signals when RTTs are sub-millisecond.
pub fn compute_reo_wnd_us(rack_aggressive: bool, min_rtt_us: u32, srtt_us: Option<u32>) -> u32 {
    if rack_aggressive {
        return 0;
    }
    match srtt_us {
        None => (min_rtt_us / 2).max(1_000),
        Some(srtt) => {
            let a = srtt / 4;
            let b = min_rtt_us / 2;
            a.min(b).max(1_000)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_on_ack_keeps_newest() {
        let mut r = RackState::new();
        r.update_on_ack(100, 500);
        r.update_on_ack(50, 400); // older xmit — ignored
        assert_eq!((r.xmit_ts_ns, r.end_seq), (100, 500));
        r.update_on_ack(200, 600); // newer xmit — taken
        assert_eq!((r.xmit_ts_ns, r.end_seq), (200, 600));
        r.update_on_ack(200, 700); // same xmit, greater seq — taken
        assert_eq!((r.xmit_ts_ns, r.end_seq), (200, 700));
    }

    #[test]
    fn detect_lost_fires_when_entry_older_and_beyond_reo_wnd() {
        let mut r = RackState::new();
        r.update_on_ack(1_000_000, 600);
        // Entry with xmit_ts=500_000 — older than RACK.xmit_ts=1_000_000.
        // now=2_000_000, reo_wnd_us=500 → age=1_500_000 ns = 1500µs > 500µs.
        assert!(r.detect_lost(500_000, 400, 2_000_000, 500));
    }

    #[test]
    fn detect_lost_false_when_within_reo_wnd() {
        let mut r = RackState::new();
        r.update_on_ack(1_000_000, 600);
        // Entry xmit_ts=500_000, now=1_000_100, reo_wnd_us=500_000 (500ms).
        // age=500_100 ns = 500.1µs << 500_000µs → not lost.
        assert!(!r.detect_lost(500_000, 400, 1_000_100, 500_000));
    }

    #[test]
    fn detect_lost_false_when_no_newer_ack() {
        let mut r = RackState::new();
        r.update_on_ack(500_000, 400);
        // Entry xmit_ts=1_000_000 — newer than RACK.xmit_ts=500_000.
        // No newer-ack-exists → not lost regardless of age.
        assert!(!r.detect_lost(1_000_000, 600, 10_000_000, 1));
    }

    #[test]
    fn aggressive_reo_wnd_is_zero() {
        assert_eq!(compute_reo_wnd_us(true, 100_000, Some(200_000)), 0);
    }

    #[test]
    fn non_aggressive_reo_wnd_min_of_srtt4_and_minrtt2() {
        // srtt/4 = 50_000, min_rtt/2 = 30_000 → min = 30_000.
        assert_eq!(compute_reo_wnd_us(false, 60_000, Some(200_000)), 30_000);
    }

    #[test]
    fn reo_wnd_floored_at_1ms() {
        // Sub-millisecond min_rtt + tiny srtt — floor at 1000µs.
        assert_eq!(compute_reo_wnd_us(false, 100, Some(400)), 1_000);
    }

    #[test]
    fn update_min_rtt_tracks_minimum() {
        let mut r = RackState::new();
        r.update_min_rtt(100);
        assert_eq!(r.min_rtt_us, 100);
        r.update_min_rtt(200); // larger — ignored
        assert_eq!(r.min_rtt_us, 100);
        r.update_min_rtt(50); // smaller — taken
        assert_eq!(r.min_rtt_us, 50);
    }
}
