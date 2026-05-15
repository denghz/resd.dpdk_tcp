//! Per-iteration attribution-bucket CSV emit (T51 deferred-work item 4).
//!
//! Phase 9 of the 2026-05-09 bench-suite overhaul (closes C-E3) added
//! the `unsupported_buckets: u32` bitfield to
//! [`crate::attribution::HwTsBuckets`] so CSV consumers could distinguish
//! "0 ns measured" from "no data" for the two Hw-mode buckets that have
//! no engine-side timestamps available
//! (`tx_sched_to_nic_tx_wire_ns`, `nic_rx_to_enqueued_ns`).
//!
//! T51's deferred-work item 4 noted that bench-rtt's CSV pipeline did
//! not surface the flag — only `rtt_ns` flowed through into the summary
//! / raw-samples CSVs, so a c7i Hw-mode run that legitimately reported
//! 0 ns in those columns would look identical to one that simply lacked
//! the engine-side probe. This module closes that gap.
//!
//! ## Schema
//!
//! Per-iteration sidecar — one row per measurement iteration, written
//! through [`bench_common::raw_samples::RawSamplesWriter`] for the
//! streaming + RFC 4180 quoting it provides. The schema is:
//!
//! ```text
//! bucket_id, iter, mode, rtt_ns, rx_hw_ts_ns,
//! user_send_to_tx_sched_ns, tx_sched_to_nic_tx_wire_ns,
//! nic_tx_wire_to_nic_rx_ns, nic_rx_to_enqueued_ns,
//! enqueued_to_user_return_ns,
//! tsc_user_send_to_tx_sched_ns, tsc_tx_sched_to_enqueued_ns,
//! tsc_enqueued_to_user_return_ns,
//! unsupported_mask
//! ```
//!
//! - `mode` is `Hw` (rx HW-TS observed) or `Tsc` (NIC zeroed it).
//! - In `Hw` rows the 5 Hw-bucket columns are populated and the 3
//!   `tsc_*` columns are blank.
//! - In `Tsc` rows the 3 `tsc_*` columns are populated and the 5
//!   Hw-bucket columns are blank.
//! - `unsupported_mask` is the raw u32 bitmask from
//!   [`crate::attribution::HwTsBuckets::unsupported_buckets`] for `Hw`
//!   rows; `Tsc` rows write `0` (the schema has no unsupported semantics).
//!
//! Emit paths today only run on the `dpdk_net` arm — `linux_kernel` and
//! `fstack` go through their own request-response loops with no
//! per-iter attribution captures. Those arms simply skip attribution
//! emit; the summary CSV remains the cross-stack comparison surface.

use crate::attribution::{AttributionMode, IterRecord};

/// Locked column header for the attribution sidecar CSV. Reordering
/// breaks downstream consumers that index by position; the integration
/// test in `tests/attribution_csv.rs` pins this list.
pub const ATTRIBUTION_CSV_HEADER: &[&str] = &[
    "bucket_id",
    "iter",
    "mode",
    "rtt_ns",
    "rx_hw_ts_ns",
    "user_send_to_tx_sched_ns",
    "tx_sched_to_nic_tx_wire_ns",
    "nic_tx_wire_to_nic_rx_ns",
    "nic_rx_to_enqueued_ns",
    "enqueued_to_user_return_ns",
    "tsc_user_send_to_tx_sched_ns",
    "tsc_tx_sched_to_enqueued_ns",
    "tsc_enqueued_to_user_return_ns",
    "unsupported_mask",
];

/// Convenience accessor returning [`ATTRIBUTION_CSV_HEADER`] as a
/// slice. Provided so the integration test can call it without a
/// const-import (clarity over micro-optimisation).
pub fn attribution_csv_header() -> &'static [&'static str] {
    ATTRIBUTION_CSV_HEADER
}

/// Build a single attribution row from an [`IterRecord`].
///
/// Returns 14 columns matching [`ATTRIBUTION_CSV_HEADER`]. Columns that
/// don't apply to the row's mode are emitted as empty strings (RFC 4180
/// missing-cell convention) — pandas, polars, and the `csv` crate all
/// parse those back as `None` / `null`.
pub fn attribution_row_cols(bucket_id: &str, iter: u64, rec: &IterRecord) -> Vec<String> {
    let blank = String::new();
    let mode = match rec.mode {
        AttributionMode::Hw => "Hw",
        AttributionMode::Tsc => "Tsc",
    };
    let rtt_ns = rec.rtt_ns.to_string();
    let rx_hw_ts_ns = rec.rx_hw_ts_ns.to_string();

    // Hw-bucket cells (5) + Tsc-bucket cells (3). Exactly one variant
    // is `Some` per `compose_iter_record`'s contract.
    let (
        user_send_to_tx_sched,
        tx_sched_to_nic_tx_wire,
        nic_tx_wire_to_nic_rx,
        nic_rx_to_enqueued,
        enqueued_to_user_return,
        tsc_user_send_to_tx_sched,
        tsc_tx_sched_to_enqueued,
        tsc_enqueued_to_user_return,
        unsupported_mask,
    ) = match (rec.hw_buckets, rec.tsc_buckets) {
        (Some(hw), None) => (
            hw.user_send_to_tx_sched_ns.to_string(),
            hw.tx_sched_to_nic_tx_wire_ns.to_string(),
            hw.nic_tx_wire_to_nic_rx_ns.to_string(),
            hw.nic_rx_to_enqueued_ns.to_string(),
            hw.enqueued_to_user_return_ns.to_string(),
            blank.clone(),
            blank.clone(),
            blank.clone(),
            hw.unsupported_buckets.to_string(),
        ),
        (None, Some(tsc)) => (
            blank.clone(),
            blank.clone(),
            blank.clone(),
            blank.clone(),
            blank.clone(),
            tsc.user_send_to_tx_sched_ns.to_string(),
            tsc.tx_sched_to_enqueued_ns.to_string(),
            tsc.enqueued_to_user_return_ns.to_string(),
            // Tsc mode has no unsupported-bucket concept by
            // construction — emit 0 so the column always parses as a
            // number, never blank.
            "0".to_string(),
        ),
        // Defensive arms — `compose_iter_record` never produces these.
        // Emit blanks rather than panicking; a malformed row is easier
        // to investigate post-hoc than a crashed bench.
        (Some(_), Some(_)) | (None, None) => (
            blank.clone(),
            blank.clone(),
            blank.clone(),
            blank.clone(),
            blank.clone(),
            blank.clone(),
            blank.clone(),
            blank.clone(),
            blank.clone(),
        ),
    };

    vec![
        bucket_id.to_string(),
        iter.to_string(),
        mode.to_string(),
        rtt_ns,
        rx_hw_ts_ns,
        user_send_to_tx_sched,
        tx_sched_to_nic_tx_wire,
        nic_tx_wire_to_nic_rx,
        nic_rx_to_enqueued,
        enqueued_to_user_return,
        tsc_user_send_to_tx_sched,
        tsc_tx_sched_to_enqueued,
        tsc_enqueued_to_user_return,
        unsupported_mask,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attribution::{HwTsBuckets, TscFallbackBuckets};

    #[test]
    fn header_length_matches_row_length() {
        let rec = IterRecord {
            rtt_ns: 1,
            rx_hw_ts_ns: 0,
            mode: AttributionMode::Tsc,
            hw_buckets: None,
            tsc_buckets: Some(TscFallbackBuckets::default()),
        };
        let cols = attribution_row_cols("b", 0, &rec);
        assert_eq!(cols.len(), ATTRIBUTION_CSV_HEADER.len());
    }

    #[test]
    fn hw_row_carries_unsupported_mask() {
        let rec = IterRecord {
            rtt_ns: 1_000,
            rx_hw_ts_ns: 12_345,
            mode: AttributionMode::Hw,
            hw_buckets: Some(HwTsBuckets {
                user_send_to_tx_sched_ns: 100,
                tx_sched_to_nic_tx_wire_ns: 0,
                nic_tx_wire_to_nic_rx_ns: 800,
                nic_rx_to_enqueued_ns: 0,
                enqueued_to_user_return_ns: 100,
                unsupported_buckets: HwTsBuckets::UNSUPPORTED_TX_SCHED_TO_NIC_TX_WIRE
                    | HwTsBuckets::UNSUPPORTED_NIC_RX_TO_ENQUEUED,
            }),
            tsc_buckets: None,
        };
        let cols = attribution_row_cols("payload_128", 17, &rec);
        let last = cols.last().unwrap();
        assert_eq!(last, "3", "unsupported_mask is the trailing column");
    }

    #[test]
    fn tsc_row_emits_blank_hw_columns() {
        let rec = IterRecord {
            rtt_ns: 500,
            rx_hw_ts_ns: 0,
            mode: AttributionMode::Tsc,
            hw_buckets: None,
            tsc_buckets: Some(TscFallbackBuckets {
                user_send_to_tx_sched_ns: 50,
                tx_sched_to_enqueued_ns: 400,
                enqueued_to_user_return_ns: 50,
            }),
        };
        let cols = attribution_row_cols("payload_64", 0, &rec);
        // Hw columns (indices 5..=9) must all be blank.
        for i in 5..=9 {
            assert!(
                cols[i].is_empty(),
                "Hw col {i} ({}) should be blank on Tsc row, got {:?}",
                ATTRIBUTION_CSV_HEADER[i],
                cols[i]
            );
        }
        // unsupported_mask defaults to "0" on Tsc rows.
        assert_eq!(cols.last().unwrap(), "0");
    }
}
