//! Attribution buckets for the end-to-end request-response RTT.
//!
//! Spec §6 decomposes one 128 B / 128 B round-trip into five
//! attribution buckets when NIC hardware receive-timestamps are
//! available (HW-TS mode), and into three collapsed buckets when they
//! are not (TSC-fallback mode).
//!
//! # HW-TS mode (5 buckets)
//!
//! Requires the peer NIC to populate `rx_hw_ts_ns` on the received
//! ACK/response mbuf via the `rte_dynfield_timestamp` dynamic field.
//! The sum of all five buckets equals the wall-clock RTT measured
//! from `rdtsc()` at `send_bytes` entry to `rdtsc()` at the Readable
//! event's caller observation, within ±50 ns (spec §6 sum-identity).
//!
//! - `user_send_to_tx_sched_ns` — application `send_bytes()` entry to
//!   the moment the engine pushes the outgoing frame onto the TX
//!   descriptor ring (pre-rte_eth_tx_burst).
//! - `tx_sched_to_nic_tx_wire_ns` — descriptor-ring push to the moment
//!   the NIC places the first bit on the wire. On mlx5/ice this can be
//!   read directly from the NIC's TX timestamp; on ENA (which does not
//!   populate TX-TS) and on every NIC the engine targets today this
//!   bucket is **flagged unsupported** — see [`HwTsBuckets::UNSUPPORTED_TX_SCHED_TO_NIC_TX_WIRE`].
//! - `nic_tx_wire_to_nic_rx_ns` — over-the-wire round-trip time, bit-
//!   on-wire at local NIC to bit-off-wire at local NIC. Includes peer
//!   echo-server processing.
//! - `nic_rx_to_enqueued_ns` — local NIC RX to the moment the engine
//!   delivers the Readable event (post-TCP-reassembly, post-
//!   `deliver_readable`). The engine doesn't expose the NIC-RX-poll
//!   anchor today, so this bucket is **flagged unsupported** — see
//!   [`HwTsBuckets::UNSUPPORTED_NIC_RX_TO_ENQUEUED`].
//! - `enqueued_to_user_return_ns` — engine-side Readable emit to the
//!   application's observation of the event via `engine.events()`.
//!
//! ## "Unsupported" vs "measured zero"
//!
//! Phase 9 of the 2026-05-09 bench-suite overhaul (closes C-E3) added
//! the [`HwTsBuckets::unsupported_buckets`] bitfield so CSV consumers
//! can distinguish a bucket the engine could not measure from a bucket
//! that genuinely measured 0 ns. An unsupported bucket's `*_ns` field
//! is held at 0 (so [`HwTsBuckets::total_ns`] still sums to the wall-
//! clock RTT) and the corresponding bit in
//! [`HwTsBuckets::unsupported_buckets`] is set. Use the
//! [`HwTsBuckets::is_tx_sched_to_nic_tx_wire_unsupported`] /
//! [`HwTsBuckets::is_nic_rx_to_enqueued_unsupported`] accessors to
//! probe.
//!
//! Live c7i validation (real DPDK TX HW-TS / engine NIC-RX probe) is
//! deferred to Phase 11+ (per the plan); Phase 9's contract is just
//! "the path no longer silently zeros buckets that should be missing".
//!
//! # TSC-fallback mode (3 buckets)
//!
//! On ENA the NIC does NOT populate `rx_hw_ts_ns` — see parent spec
//! §10.5 (`offload_missing_rx_timestamp` bumped one-shot at bring-up;
//! every per-event `rx_hw_ts_ns == 0`). The wire + full-RX path is
//! merged into one `tx_sched_to_enqueued_ns` bucket derived from
//! `rdtsc()` deltas inside the engine's RX dispatch.
//!
//! - `user_send_to_tx_sched_ns` — same as HW-TS mode.
//! - `tx_sched_to_enqueued_ns` — collapsed wire + peer-echo + local-
//!   NIC-RX + engine reassembly.
//! - `enqueued_to_user_return_ns` — same as HW-TS mode.
//!
//! # Invariants
//!
//! - Every field is `u64` nanoseconds. TSC-derived deltas use the
//!   process-wide `tsc_epoch()` calibration (spec §7.5) so rollover +
//!   wrap semantics match `dpdk_net_core::clock::now_ns()`.
//! - `total_ns()` uses saturating addition. Attribution buckets are
//!   small (typically microseconds on a well-tuned host), so
//!   saturation fires only on programming errors; the explicit
//!   saturation keeps overflow from silently poisoning a run.
//! - Bucket-ordering is not implicit in the sum — the Readable event's
//!   `rx_hw_ts_ns` and the engine's `emitted_ts_ns` are produced by
//!   independent sources (NIC clock vs. host TSC) and can disagree
//!   under calibration drift. The caller composes the buckets from
//!   raw timestamps; `total_ns()` only arithmetic-sums.

/// Five-bucket attribution vector for the HW-TS mode. See the module-
/// level doc comment for the field semantics. All buckets are u64 ns.
///
/// [`unsupported_buckets`] is a bitfield identifying buckets whose
/// `*_ns` fields are held at zero because the engine can't measure
/// them on the current host (no DPDK TX HW-TS; no engine NIC-RX-to-
/// enqueued probe). CSV consumers check the relevant bit before
/// interpreting a 0 in those columns as "measured 0 ns".
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct HwTsBuckets {
    pub user_send_to_tx_sched_ns: u64,
    pub tx_sched_to_nic_tx_wire_ns: u64,
    pub nic_tx_wire_to_nic_rx_ns: u64,
    pub nic_rx_to_enqueued_ns: u64,
    pub enqueued_to_user_return_ns: u64,
    /// Bitmask of unsupported buckets. A set bit means the
    /// corresponding `*_ns` field is held at zero because the engine
    /// can't measure that span on the current host — distinct from a
    /// bucket that genuinely measured 0 ns.
    pub unsupported_buckets: u32,
}

impl HwTsBuckets {
    /// Bit set in [`unsupported_buckets`] when
    /// [`tx_sched_to_nic_tx_wire_ns`] cannot be measured (no DPDK TX
    /// HW-TS available on the engine). 0 in the corresponding CSV
    /// column means "missing data", NOT "measured zero".
    pub const UNSUPPORTED_TX_SCHED_TO_NIC_TX_WIRE: u32 = 1 << 0;

    /// Bit set in [`unsupported_buckets`] when
    /// [`nic_rx_to_enqueued_ns`] cannot be measured (no engine-side
    /// NIC-RX-poll anchor exposed). 0 in the corresponding CSV column
    /// means "missing data", NOT "measured zero".
    pub const UNSUPPORTED_NIC_RX_TO_ENQUEUED: u32 = 1 << 1;

    /// Sum of all five buckets in ns. Saturating addition — see the
    /// module-level note on invariants. Unsupported buckets hold
    /// `*_ns == 0`, so they contribute 0 to the sum and the wall-clock
    /// RTT identity holds even when the bucket is flagged unsupported.
    pub fn total_ns(&self) -> u64 {
        self.user_send_to_tx_sched_ns
            .saturating_add(self.tx_sched_to_nic_tx_wire_ns)
            .saturating_add(self.nic_tx_wire_to_nic_rx_ns)
            .saturating_add(self.nic_rx_to_enqueued_ns)
            .saturating_add(self.enqueued_to_user_return_ns)
    }

    /// True if [`tx_sched_to_nic_tx_wire_ns`] is held at zero because
    /// the engine doesn't expose a DPDK TX HW-TS for the current
    /// burst. CSV consumers should treat the column as missing data
    /// (not "measured 0 ns") in this case.
    pub fn is_tx_sched_to_nic_tx_wire_unsupported(&self) -> bool {
        self.unsupported_buckets & Self::UNSUPPORTED_TX_SCHED_TO_NIC_TX_WIRE != 0
    }

    /// True if [`nic_rx_to_enqueued_ns`] is held at zero because the
    /// engine doesn't expose a NIC-RX-poll-to-enqueued anchor on the
    /// current host. CSV consumers should treat the column as missing
    /// data (not "measured 0 ns") in this case.
    pub fn is_nic_rx_to_enqueued_unsupported(&self) -> bool {
        self.unsupported_buckets & Self::UNSUPPORTED_NIC_RX_TO_ENQUEUED != 0
    }
}

/// Three-bucket attribution vector for the TSC-fallback mode (used on
/// ENA where `rx_hw_ts_ns` is always 0). The middle bucket collapses
/// the over-the-wire + peer-echo + local-NIC-RX + engine-reassembly
/// segments into one measurable span, derived from host-TSC deltas.
/// All buckets are u64 ns.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct TscFallbackBuckets {
    pub user_send_to_tx_sched_ns: u64,
    pub tx_sched_to_enqueued_ns: u64,
    pub enqueued_to_user_return_ns: u64,
}

impl TscFallbackBuckets {
    /// Sum of all three buckets in ns. Saturating addition — see the
    /// module-level note on invariants.
    pub fn total_ns(&self) -> u64 {
        self.user_send_to_tx_sched_ns
            .saturating_add(self.tx_sched_to_enqueued_ns)
            .saturating_add(self.enqueued_to_user_return_ns)
    }
}

/// Discriminator for which attribution bucket set was used on a given
/// round-trip. `Hw` iff the Readable event carried a non-zero
/// `rx_hw_ts_ns`; `Tsc` otherwise. Exposed to the CSV emitter so a
/// downstream consumer can tell the two schemas apart without peeking
/// at values.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttributionMode {
    /// NIC hardware receive-timestamp observed — 5 buckets.
    Hw,
    /// NIC did not populate `rx_hw_ts_ns` — 3 buckets.
    Tsc,
}

impl AttributionMode {
    /// Select the mode based on the observed `rx_hw_ts_ns` on the
    /// Readable event. `0` → `Tsc` (ENA steady state); any non-zero
    /// value → `Hw` (mlx5 / ice / future-gen ENA).
    pub fn from_rx_hw_ts(rx_hw_ts_ns: u64) -> Self {
        if rx_hw_ts_ns == 0 {
            AttributionMode::Tsc
        } else {
            AttributionMode::Hw
        }
    }
}

// ---------------------------------------------------------------------------
// Pure-Rust composition primitive (Phase 9, closes C-E3).
//
// Factored out of `workload::request_response_attributed`'s tail so it
// can be unit-tested without a live DPDK engine. The composition logic
// is the contract: given raw TSC anchors + tsc_hz + rx_hw_ts_ns, build
// the IterRecord with the right bucket variant and the right
// unsupported flags.
// ---------------------------------------------------------------------------

/// Raw inputs to [`compose_iter_record`]: the four TSC anchors
/// captured around one request-response round-trip, the rx HW-TS
/// observed on the Readable event (0 on ENA), and the process-wide
/// `tsc_hz` calibration.
#[derive(Debug, Clone, Copy)]
pub struct IterInputs {
    /// `rdtsc()` at `send_bytes()` entry.
    pub t_user_send: u64,
    /// `rdtsc()` after the engine accepts the full request payload
    /// (post-`send_bytes` loop, pre-RX wait).
    pub t_tx_sched: u64,
    /// `rdtsc()` at the moment the engine has delivered enough
    /// Readable bytes to satisfy the response.
    pub t_enqueued: u64,
    /// `rdtsc()` immediately before the workload returns (after the
    /// full response is observed).
    pub t_user_return: u64,
    /// Raw `rx_hw_ts_ns` from the latest Readable event seen during
    /// the RX drain. 0 on ENA; non-zero on mlx5 / ice / c7i.
    pub rx_hw_ts_ns: u64,
    /// Process-wide TSC frequency (`rte_get_tsc_hz()` on DPDK paths,
    /// or any host TSC calibration).
    pub tsc_hz: u64,
}

/// The per-iteration measurement product: the wall-clock RTT, the raw
/// rx HW-TS, the discriminator [`AttributionMode`], and one of the two
/// bucket variants (the other is `None`).
///
/// This is the single record [`compose_iter_record`] returns and that
/// `workload::request_response_attributed` re-exports as
/// `bench_rtt::workload::IterRecord` for back-compat.
#[derive(Debug, Clone, Copy)]
pub struct IterRecord {
    pub rtt_ns: u64,
    pub rx_hw_ts_ns: u64,
    pub mode: AttributionMode,
    pub hw_buckets: Option<HwTsBuckets>,
    pub tsc_buckets: Option<TscFallbackBuckets>,
}

/// Convert a TSC-cycle delta to nanoseconds. u128 intermediate to
/// avoid overflow at realistic durations. Mirrored from
/// [`crate::workload::tsc_delta_to_ns`] — duplicated here so this
/// module stays self-contained for the unit tests in
/// `tests/attribution_hw_path.rs`.
fn tsc_delta_to_ns_local(t0: u64, t1: u64, tsc_hz: u64) -> u64 {
    let delta = t1.wrapping_sub(t0);
    ((delta as u128).saturating_mul(1_000_000_000u128) / tsc_hz as u128) as u64
}

/// Build the per-iteration [`IterRecord`] from raw TSC anchors.
///
/// Composition rules:
/// - `mode = AttributionMode::from_rx_hw_ts(inputs.rx_hw_ts_ns)`.
/// - `rtt_ns` is `t_user_return - t_user_send` in ns.
/// - In `Hw` mode, the three measurable buckets
///   (`user_send_to_tx_sched_ns`, `nic_tx_wire_to_nic_rx_ns`,
///   `enqueued_to_user_return_ns`) populate from the three TSC deltas;
///   the two unmeasurable buckets (`tx_sched_to_nic_tx_wire_ns`,
///   `nic_rx_to_enqueued_ns`) are held at zero and flagged in
///   [`HwTsBuckets::unsupported_buckets`]. **Rationale:** the engine
///   doesn't expose a DPDK TX HW-TS or a NIC-RX-poll anchor today;
///   silent zeros in those columns would be indistinguishable from
///   measured zeros, defeating the decomposition's purpose. Phase 11+
///   will wire real measurements once the engine exposes them.
/// - In `Tsc` mode, all three TSC-fallback buckets populate from TSC
///   deltas — no unsupported flags apply (the schema only has three
///   buckets, all measurable from host TSC).
///
/// In both modes `total_ns()` of the populated bucket variant equals
/// `rtt_ns`, modulo the saturating-add invariant in
/// [`HwTsBuckets::total_ns`] / [`TscFallbackBuckets::total_ns`].
pub fn compose_iter_record(inputs: IterInputs) -> IterRecord {
    let IterInputs {
        t_user_send,
        t_tx_sched,
        t_enqueued,
        t_user_return,
        rx_hw_ts_ns,
        tsc_hz,
    } = inputs;

    let rtt_ns = tsc_delta_to_ns_local(t_user_send, t_user_return, tsc_hz);
    let mode = AttributionMode::from_rx_hw_ts(rx_hw_ts_ns);

    let (hw_buckets, tsc_buckets) = match mode {
        AttributionMode::Hw => {
            let host_span_ns = tsc_delta_to_ns_local(t_tx_sched, t_enqueued, tsc_hz);
            let bucket_a = tsc_delta_to_ns_local(t_user_send, t_tx_sched, tsc_hz);
            let bucket_e = tsc_delta_to_ns_local(t_enqueued, t_user_return, tsc_hz);
            (
                Some(HwTsBuckets {
                    user_send_to_tx_sched_ns: bucket_a,
                    // Held at zero — see UNSUPPORTED_TX_SCHED_TO_NIC_TX_WIRE.
                    tx_sched_to_nic_tx_wire_ns: 0,
                    nic_tx_wire_to_nic_rx_ns: host_span_ns,
                    // Held at zero — see UNSUPPORTED_NIC_RX_TO_ENQUEUED.
                    nic_rx_to_enqueued_ns: 0,
                    enqueued_to_user_return_ns: bucket_e,
                    unsupported_buckets: HwTsBuckets::UNSUPPORTED_TX_SCHED_TO_NIC_TX_WIRE
                        | HwTsBuckets::UNSUPPORTED_NIC_RX_TO_ENQUEUED,
                }),
                None,
            )
        }
        AttributionMode::Tsc => {
            let bucket_a = tsc_delta_to_ns_local(t_user_send, t_tx_sched, tsc_hz);
            let bucket_b = tsc_delta_to_ns_local(t_tx_sched, t_enqueued, tsc_hz);
            let bucket_c = tsc_delta_to_ns_local(t_enqueued, t_user_return, tsc_hz);
            (
                None,
                Some(TscFallbackBuckets {
                    user_send_to_tx_sched_ns: bucket_a,
                    tx_sched_to_enqueued_ns: bucket_b,
                    enqueued_to_user_return_ns: bucket_c,
                }),
            )
        }
    };

    IterRecord {
        rtt_ns,
        rx_hw_ts_ns,
        mode,
        hw_buckets,
        tsc_buckets,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hw_buckets_total_sums_all_five() {
        let b = HwTsBuckets {
            user_send_to_tx_sched_ns: 100,
            tx_sched_to_nic_tx_wire_ns: 200,
            nic_tx_wire_to_nic_rx_ns: 10_000,
            nic_rx_to_enqueued_ns: 50,
            enqueued_to_user_return_ns: 80,
            unsupported_buckets: 0,
        };
        assert_eq!(b.total_ns(), 10_430);
    }

    #[test]
    fn tsc_buckets_total_sums_all_three() {
        let b = TscFallbackBuckets {
            user_send_to_tx_sched_ns: 100,
            tx_sched_to_enqueued_ns: 10_250,
            enqueued_to_user_return_ns: 80,
        };
        assert_eq!(b.total_ns(), 10_430);
    }

    #[test]
    fn hw_buckets_default_is_zero() {
        assert_eq!(HwTsBuckets::default().total_ns(), 0);
    }

    #[test]
    fn tsc_buckets_default_is_zero() {
        assert_eq!(TscFallbackBuckets::default().total_ns(), 0);
    }

    #[test]
    fn hw_buckets_saturate_on_overflow() {
        // If any caller programming error pushes a per-bucket u64 high,
        // total_ns must saturate at u64::MAX rather than silently wrap.
        let b = HwTsBuckets {
            user_send_to_tx_sched_ns: u64::MAX,
            tx_sched_to_nic_tx_wire_ns: 1,
            nic_tx_wire_to_nic_rx_ns: 0,
            nic_rx_to_enqueued_ns: 0,
            enqueued_to_user_return_ns: 0,
            unsupported_buckets: 0,
        };
        assert_eq!(b.total_ns(), u64::MAX);
    }

    #[test]
    fn attribution_mode_selects_on_rx_hw_ts_zero() {
        assert_eq!(AttributionMode::from_rx_hw_ts(0), AttributionMode::Tsc);
        assert_eq!(AttributionMode::from_rx_hw_ts(1), AttributionMode::Hw);
        assert_eq!(
            AttributionMode::from_rx_hw_ts(u64::MAX),
            AttributionMode::Hw
        );
    }

    #[test]
    fn unsupported_flags_default_clear() {
        let b = HwTsBuckets::default();
        assert!(!b.is_tx_sched_to_nic_tx_wire_unsupported());
        assert!(!b.is_nic_rx_to_enqueued_unsupported());
    }

    #[test]
    fn unsupported_flags_independent_bits() {
        // Setting one bit must not affect the other accessor.
        let mut b = HwTsBuckets::default();
        b.unsupported_buckets = HwTsBuckets::UNSUPPORTED_TX_SCHED_TO_NIC_TX_WIRE;
        assert!(b.is_tx_sched_to_nic_tx_wire_unsupported());
        assert!(!b.is_nic_rx_to_enqueued_unsupported());

        b.unsupported_buckets = HwTsBuckets::UNSUPPORTED_NIC_RX_TO_ENQUEUED;
        assert!(!b.is_tx_sched_to_nic_tx_wire_unsupported());
        assert!(b.is_nic_rx_to_enqueued_unsupported());

        b.unsupported_buckets = HwTsBuckets::UNSUPPORTED_TX_SCHED_TO_NIC_TX_WIRE
            | HwTsBuckets::UNSUPPORTED_NIC_RX_TO_ENQUEUED;
        assert!(b.is_tx_sched_to_nic_tx_wire_unsupported());
        assert!(b.is_nic_rx_to_enqueued_unsupported());
    }
}
