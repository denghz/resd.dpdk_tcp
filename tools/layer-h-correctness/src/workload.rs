//! Spec §5.4: per-scenario lifecycle + deadline-driven outer loop.
//!
//! `run_one_scenario` is the engine-driven entry point. The pure-data
//! helper (`select_counter_names`) is exposed and unit-tested without
//! DPDK.

use std::time::Duration;

use crate::counters_snapshot::{Snapshot, SIDE_CHECK_COUNTERS};
use crate::observation::{EventRing, Verdict};
use crate::scenarios::LayerHScenario;

/// Spec §5.4 constants.
pub const WARMUP_ITERS: u64 = 100;
pub const OBSERVATION_BATCH: u64 = 100;
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// Build the union of every counter referenced by the selection's
/// expectations, plus the global side-check names. Used at startup to
/// pre-resolve every name through `lookup_counter`.
pub fn select_counter_names(selection: &[&LayerHScenario]) -> Vec<&'static str> {
    use std::collections::BTreeSet;
    let mut set: BTreeSet<&'static str> = BTreeSet::new();
    for s in selection {
        for (name, _) in s.counter_expectations {
            set.insert(*name);
        }
        for (group, _) in s.disjunctive_expectations {
            for n in *group {
                set.insert(*n);
            }
        }
    }
    for n in SIDE_CHECK_COUNTERS {
        set.insert(*n);
    }
    set.into_iter().collect()
}

/// Per-scenario aggregated verdict + the data needed for the failure
/// bundle (snapshots, drained event window).
#[derive(Debug)]
pub struct ScenarioResult {
    pub scenario_name: &'static str,
    pub duration_observed: Duration,
    pub snapshot_pre: Snapshot,
    pub snapshot_post: Snapshot,
    pub verdict: Verdict,
    pub event_ring: EventRing,
}

/// Engine-driven per-scenario runner (spec §5.4 lifecycle).
#[cfg(not(test))]
#[allow(clippy::too_many_arguments)]
pub fn run_one_scenario(
    engine: &dpdk_net_core::engine::Engine,
    scenario: &LayerHScenario,
    counter_names: &[&'static str],
    peer_ip: u32,
    peer_port: u16,
    request_bytes: usize,
    response_bytes: usize,
    tsc_hz: u64,
    duration_override: Option<Duration>,
) -> anyhow::Result<ScenarioResult> {
    use std::sync::atomic::Ordering;
    use std::time::Instant;

    use anyhow::Context as _;
    use bench_rtt::workload::{open_connection, run_rtt_workload};

    use crate::assertions::{
        evaluate_counter_expectations, evaluate_disjunctive,
        evaluate_global_side_checks,
    };
    use crate::counters_snapshot;
    use crate::observation::{FailureReason, ObserveOutcome};

    let mut event_ring = EventRing::new();
    let scenario_name = scenario.name;
    let started_at = Instant::now();

    eprintln!("layer-h: scenario {scenario_name}");

    // 1. Open a connection. ConnectFailed is a verdict failure, not a
    // process error.
    let conn = match open_connection(engine, peer_ip, peer_port) {
        Ok(c) => c,
        Err(e) => {
            return Ok(ScenarioResult {
                scenario_name,
                duration_observed: started_at.elapsed(),
                snapshot_pre: Snapshot::new(),
                snapshot_post: Snapshot::new(),
                verdict: Verdict::Fail {
                    failures: vec![FailureReason::ConnectFailed { error: e.to_string() }],
                },
                event_ring,
            });
        }
    };

    // 2. Warmup. Samples + events drained on the floor; observation off.
    let _ = run_rtt_workload(
        engine, conn, request_bytes, response_bytes, tsc_hz, 0, WARMUP_ITERS,
    )
    .with_context(|| format!("warmup workload for scenario {scenario_name}"))?;

    // Discard handshake/cwnd-warmup events so they don't leak into the
    // assertion window's event_ring.
    let _ = engine.drain_events(u32::MAX, |_, _| {});

    // 3. Snapshot pre.
    let counters = engine.counters();
    let snapshot_pre = counters_snapshot::snapshot(counters, counter_names)
        .with_context(|| format!("pre snapshot for scenario {scenario_name}"))?;
    let mut obs_dropped_pre = counters.obs.events_dropped.load(Ordering::Relaxed);

    // 4. Inner loop until deadline. Fail-fast on observation failure;
    // collect counter-delta failures at end-of-scenario regardless.
    let scenario_dur = duration_override.unwrap_or(scenario.duration);
    let deadline = Instant::now() + scenario_dur;
    let mut fail_fast_failures: Vec<FailureReason> = Vec::new();
    let mut workload_error: Option<FailureReason> = None;

    while Instant::now() < deadline {
        match run_rtt_workload(
            engine, conn, request_bytes, response_bytes, tsc_hz, 0,
            OBSERVATION_BATCH,
        ) {
            Ok((_samples, _failed)) => {}
            Err(e) => {
                workload_error = Some(FailureReason::WorkloadError {
                    error: e.to_string(),
                });
                break;
            }
        }
        let outcome = crate::observation::observe_batch(
            engine, conn, &mut event_ring, obs_dropped_pre,
        );
        match outcome {
            ObserveOutcome::Ok => {
                // Refresh per-batch baseline so the next batch's delta
                // is local. `obs.events_dropped` is monotonic, so the
                // delta-this-batch question is "did any new drops
                // happen since this snapshot?".
                obs_dropped_pre =
                    engine.counters().obs.events_dropped.load(Ordering::Relaxed);
            }
            ObserveOutcome::Fail(f) => {
                fail_fast_failures.push(f);
                break;
            }
        }
    }

    // 5. Snapshot post + collect-all delta failures.
    let snapshot_post = counters_snapshot::snapshot(engine.counters(), counter_names)
        .with_context(|| format!("post snapshot for scenario {scenario_name}"))?;

    let mut all_failures = fail_fast_failures;
    if let Some(e) = workload_error {
        all_failures.push(e);
    }
    all_failures.extend(evaluate_counter_expectations(
        &snapshot_pre,
        &snapshot_post,
        scenario.counter_expectations,
    ));
    all_failures.extend(evaluate_disjunctive(
        &snapshot_pre,
        &snapshot_post,
        scenario.disjunctive_expectations,
    ));
    all_failures.extend(evaluate_global_side_checks(&snapshot_pre, &snapshot_post));

    // 6. Close (transition out of ESTABLISHED is OK — assertion window
    // already closed). Best-effort; engine error here doesn't fail the
    // verdict (the close-side errors are A6 territory and orthogonal
    // to layer-h adversity assertions).
    let _ = engine.close_conn(conn);

    let verdict = if all_failures.is_empty() {
        Verdict::Pass
    } else {
        Verdict::Fail { failures: all_failures }
    };

    Ok(ScenarioResult {
        scenario_name,
        duration_observed: started_at.elapsed(),
        snapshot_pre,
        snapshot_post,
        verdict,
        event_ring,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scenarios::MATRIX;

    #[test]
    fn select_counter_names_unions_all_expectations_plus_side_checks() {
        // Pick row 14 (corruption: only obs.events_dropped + disjunctive
        // [eth.rx_drop_cksum_bad, ip.rx_csum_bad]) plus row 1 (delay_20ms:
        // tcp.tx_retrans, tcp.tx_rto, obs.events_dropped). The union
        // must cover all four named counters plus the side-check pair.
        let row1 = MATRIX.iter().find(|s| s.name == "delay_20ms").unwrap();
        let row14 = MATRIX
            .iter()
            .find(|s| s.name == "corruption_001pct")
            .unwrap();
        let names = select_counter_names(&[row1, row14]);
        for expected in [
            "tcp.tx_retrans",
            "tcp.tx_rto",
            "obs.events_dropped",
            "eth.rx_drop_cksum_bad",
            "ip.rx_csum_bad",
            "tcp.mbuf_refcnt_drop_unexpected",
        ] {
            assert!(
                names.contains(&expected),
                "expected {expected} in names {names:?}"
            );
        }
    }

    #[test]
    fn select_counter_names_dedupes() {
        // Row 8 (loss_1pct) lists `tcp.tx_retrans` twice (`>0` and
        // `<=50000`). Selection should dedupe.
        let row8 = MATRIX.iter().find(|s| s.name == "loss_1pct").unwrap();
        let names = select_counter_names(&[row8]);
        let count = names.iter().filter(|n| **n == "tcp.tx_retrans").count();
        assert_eq!(count, 1);
    }

    #[test]
    fn warmup_iters_is_100() {
        assert_eq!(WARMUP_ITERS, 100);
    }

    #[test]
    fn observation_batch_is_100() {
        assert_eq!(OBSERVATION_BATCH, 100);
    }

    #[test]
    fn connect_timeout_is_10s() {
        assert_eq!(CONNECT_TIMEOUT, Duration::from_secs(10));
    }
}
