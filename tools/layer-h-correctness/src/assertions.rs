//! Spec §5.1: relation language for counter expectations.
//!
//! Three relations are accepted in matrix rows: `>0` (counter must
//! increase), `==0` (counter must not change), `<=N` (counter delta must
//! not exceed N). Pre-flight at driver startup parses every row's
//! relation strings; unknown literals fail at startup, never mid-sweep.

use std::fmt;

/// Counter-delta relation parsed from a matrix row's relation string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    /// `">0"` — `delta > 0`.
    GreaterThanZero,
    /// `"==0"` — `delta == 0`.
    EqualsZero,
    /// `"<=N"` — `0 ≤ delta ≤ N`. Negative deltas (impossible on
    /// monotonic u64 counters but defensively checked) fail.
    LessOrEqualThan(u64),
}

impl Relation {
    /// Parse a relation literal. Whitespace inside the literal is
    /// rejected (matrix strings are tightly formatted); the bound on
    /// `<=N` is parsed as a base-10 u64.
    pub fn parse(s: &str) -> Result<Self, RelationParseError> {
        match s {
            ">0" => Ok(Self::GreaterThanZero),
            "==0" => Ok(Self::EqualsZero),
            s if s.starts_with("<=") => {
                let n_str = &s[2..];
                let n: u64 = n_str
                    .parse()
                    .map_err(|_| RelationParseError::InvalidBound(s.to_string()))?;
                Ok(Self::LessOrEqualThan(n))
            }
            _ => Err(RelationParseError::Unknown(s.to_string())),
        }
    }

    /// Apply the relation to a delta. Returns `Ok(())` on pass, `Err`
    /// with a diagnostic on fail. `i128` so a hypothetical negative
    /// delta (impossible on u64 but defensively typed) surfaces as a
    /// fail rather than wrapping.
    pub fn check(self, counter: &str, delta: i128) -> Result<(), String> {
        match self {
            Self::GreaterThanZero => {
                if delta > 0 {
                    Ok(())
                } else {
                    Err(format!("{counter}: expected delta > 0, got {delta}"))
                }
            }
            Self::EqualsZero => {
                if delta == 0 {
                    Ok(())
                } else {
                    Err(format!("{counter}: expected delta == 0, got {delta}"))
                }
            }
            Self::LessOrEqualThan(n) => {
                if delta < 0 {
                    Err(format!(
                        "{counter}: expected 0 ≤ delta ≤ {n}, got negative {delta}"
                    ))
                } else if (delta as u128) <= n as u128 {
                    Ok(())
                } else {
                    Err(format!(
                        "{counter}: expected delta <= {n}, got {delta}"
                    ))
                }
            }
        }
    }
}

impl fmt::Display for Relation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GreaterThanZero => write!(f, ">0"),
            Self::EqualsZero => write!(f, "==0"),
            Self::LessOrEqualThan(n) => write!(f, "<={n}"),
        }
    }
}

/// Errors surfaced by `Relation::parse`. Both variants surface at driver
/// startup before the sweep begins.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RelationParseError {
    #[error("unknown relation literal: {0:?}")]
    Unknown(String),
    #[error("invalid bound on `<=N` relation: {0:?}")]
    InvalidBound(String),
}

use crate::counters_snapshot::{delta as snapshot_delta, Snapshot};
use crate::observation::FailureReason;

/// Evaluate every `(counter_name, relation_str)` pair in `expectations`
/// against `(pre, post)` snapshots. Collects all failures rather than
/// short-circuiting — the caller surfaces them together in the
/// per-scenario verdict.
pub fn evaluate_counter_expectations(
    pre: &Snapshot,
    post: &Snapshot,
    expectations: &[(&str, &str)],
) -> Vec<FailureReason> {
    let mut out = Vec::new();
    for (counter, rel_str) in expectations {
        let rel = match Relation::parse(rel_str) {
            Ok(r) => r,
            Err(e) => {
                // Should not occur — pre-flight at startup parses every
                // matrix relation. Surface as a synthetic failure so a
                // logic regression doesn't silently swallow.
                out.push(FailureReason::CounterRelation {
                    counter: (*counter).to_string(),
                    relation: (*rel_str).to_string(),
                    observed_delta: 0,
                    message: format!("relation parse error mid-sweep: {e}"),
                });
                continue;
            }
        };
        let delta = match snapshot_delta(pre, post, counter) {
            Ok(d) => d,
            Err(e) => {
                out.push(FailureReason::CounterRelation {
                    counter: (*counter).to_string(),
                    relation: rel.to_string(),
                    observed_delta: 0,
                    message: format!("counter missing from snapshot: {e}"),
                });
                continue;
            }
        };
        if let Err(msg) = rel.check(counter, delta) {
            out.push(FailureReason::CounterRelation {
                counter: (*counter).to_string(),
                relation: rel.to_string(),
                observed_delta: delta,
                message: msg,
            });
        }
    }
    out
}

/// Evaluate disjunctive groups: each `(counters[], relation)` pair
/// passes iff at least one counter in `counters[]` satisfies `relation`.
/// Used for offload-aware corruption-counter selection (spec §4 row 14).
pub fn evaluate_disjunctive(
    pre: &Snapshot,
    post: &Snapshot,
    expectations: &[(&[&str], &str)],
) -> Vec<FailureReason> {
    let mut out = Vec::new();
    for (counters, rel_str) in expectations {
        let rel = match Relation::parse(rel_str) {
            Ok(r) => r,
            Err(e) => {
                out.push(FailureReason::DisjunctiveCounterRelation {
                    counters: counters.iter().map(|s| (*s).to_string()).collect(),
                    relation: (*rel_str).to_string(),
                    observed_deltas: vec![],
                    message: format!("relation parse error mid-sweep: {e}"),
                });
                continue;
            }
        };
        let mut deltas = Vec::with_capacity(counters.len());
        let mut any_pass = false;
        for c in *counters {
            let d = snapshot_delta(pre, post, c).unwrap_or(0);
            deltas.push(d);
            if rel.check(c, d).is_ok() {
                any_pass = true;
            }
        }
        if !any_pass {
            out.push(FailureReason::disjunctive(counters, rel, &deltas));
        }
    }
    out
}

/// Evaluate the global side-checks (spec §4 "Global side-checks"):
///   - `tcp.mbuf_refcnt_drop_unexpected` delta `== 0`.
///   - `obs.events_dropped` delta `== 0`.
///
/// The per-batch live `tcp.rx_mempool_avail >= MIN` and per-batch
/// `obs.events_dropped == 0` are evaluated by `observe_batch` during
/// the run; the end-of-scenario versions are evaluated here.
pub fn evaluate_global_side_checks(pre: &Snapshot, post: &Snapshot) -> Vec<FailureReason> {
    let mut out = Vec::new();
    for counter in ["tcp.mbuf_refcnt_drop_unexpected", "obs.events_dropped"] {
        let d = match snapshot_delta(pre, post, counter) {
            Ok(d) => d,
            Err(e) => {
                out.push(FailureReason::CounterRelation {
                    counter: counter.to_string(),
                    relation: "==0".into(),
                    observed_delta: 0,
                    message: format!("counter missing from snapshot: {e}"),
                });
                continue;
            }
        };
        if d != 0 {
            out.push(FailureReason::counter_relation(
                counter,
                Relation::EqualsZero,
                d,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_known_relations() {
        assert_eq!(Relation::parse(">0").unwrap(), Relation::GreaterThanZero);
        assert_eq!(Relation::parse("==0").unwrap(), Relation::EqualsZero);
        assert_eq!(
            Relation::parse("<=0").unwrap(),
            Relation::LessOrEqualThan(0)
        );
        assert_eq!(
            Relation::parse("<=1").unwrap(),
            Relation::LessOrEqualThan(1)
        );
        assert_eq!(
            Relation::parse("<=10000").unwrap(),
            Relation::LessOrEqualThan(10_000)
        );
        assert_eq!(
            Relation::parse("<=18446744073709551615").unwrap(),
            Relation::LessOrEqualThan(u64::MAX)
        );
    }

    #[test]
    fn parse_rejects_malformed_bounds() {
        assert!(matches!(
            Relation::parse("<="),
            Err(RelationParseError::InvalidBound(_))
        ));
        assert!(matches!(
            Relation::parse("<= 1"),
            Err(RelationParseError::InvalidBound(_))
        ));
        assert!(matches!(
            Relation::parse("<=-1"),
            Err(RelationParseError::InvalidBound(_))
        ));
        assert!(matches!(
            Relation::parse("<=18446744073709551616"),
            Err(RelationParseError::InvalidBound(_))
        ));
    }

    #[test]
    fn parse_rejects_unknown_literal() {
        assert!(matches!(
            Relation::parse(""),
            Err(RelationParseError::Unknown(_))
        ));
        assert!(matches!(
            Relation::parse(">="),
            Err(RelationParseError::Unknown(_))
        ));
        assert!(matches!(
            Relation::parse("=="),
            Err(RelationParseError::Unknown(_))
        ));
    }

    #[test]
    fn greater_than_zero_truth_table() {
        assert!(Relation::GreaterThanZero.check("c", 1).is_ok());
        assert!(Relation::GreaterThanZero.check("c", 1_000).is_ok());
        assert!(Relation::GreaterThanZero.check("c", 0).is_err());
        assert!(Relation::GreaterThanZero.check("c", -1).is_err());
    }

    #[test]
    fn equals_zero_truth_table() {
        assert!(Relation::EqualsZero.check("c", 0).is_ok());
        assert!(Relation::EqualsZero.check("c", 1).is_err());
        assert!(Relation::EqualsZero.check("c", -1).is_err());
    }

    #[test]
    fn less_or_equal_truth_table() {
        let r = Relation::LessOrEqualThan(10);
        assert!(r.check("c", 0).is_ok());
        assert!(r.check("c", 1).is_ok());
        assert!(r.check("c", 10).is_ok());
        assert!(r.check("c", 11).is_err());
        assert!(r.check("c", 1_000_000).is_err());
        assert!(r.check("c", -1).is_err());
    }

    #[test]
    fn less_or_equal_at_u64_max_does_not_overflow() {
        let r = Relation::LessOrEqualThan(u64::MAX);
        // delta is i128 so it can hold u64::MAX without overflow.
        assert!(r.check("c", u64::MAX as i128).is_ok());
        assert!(r.check("c", (u64::MAX as i128) + 1).is_err());
        assert!(r.check("c", -1).is_err());
    }

    #[test]
    fn display_round_trips() {
        for s in [">0", "==0", "<=0", "<=42"] {
            let r = Relation::parse(s).unwrap();
            assert_eq!(format!("{r}"), s);
        }
    }
}

#[cfg(test)]
mod evaluator_tests {
    use super::*;
    use crate::counters_snapshot::Snapshot;

    fn snap(pairs: &[(&str, u64)]) -> Snapshot {
        pairs.iter().map(|(k, v)| ((*k).to_string(), *v)).collect()
    }

    #[test]
    fn evaluate_passes_when_all_expectations_hold() {
        let pre = snap(&[("tcp.tx_retrans", 0), ("obs.events_dropped", 0)]);
        let post = snap(&[("tcp.tx_retrans", 5), ("obs.events_dropped", 0)]);
        let exp = &[
            ("tcp.tx_retrans", ">0"),
            ("tcp.tx_retrans", "<=10000"),
            ("obs.events_dropped", "==0"),
        ];
        let fails = evaluate_counter_expectations(&pre, &post, exp);
        assert!(fails.is_empty(), "expected pass, got {fails:?}");
    }

    #[test]
    fn evaluate_collects_all_failures_not_first() {
        let pre = snap(&[("tcp.tx_retrans", 0), ("obs.events_dropped", 0)]);
        let post = snap(&[("tcp.tx_retrans", 0), ("obs.events_dropped", 5)]);
        let exp = &[
            ("tcp.tx_retrans", ">0"),       // fail: delta=0
            ("obs.events_dropped", "==0"),  // fail: delta=5
        ];
        let fails = evaluate_counter_expectations(&pre, &post, exp);
        assert_eq!(fails.len(), 2);
    }

    #[test]
    fn disjunctive_passes_when_any_counter_fires() {
        let pre = snap(&[("eth.rx_drop_cksum_bad", 0), ("ip.rx_csum_bad", 0)]);
        let post = snap(&[("eth.rx_drop_cksum_bad", 0), ("ip.rx_csum_bad", 7)]);
        let exp: &[(&[&str], &str)] =
            &[(&["eth.rx_drop_cksum_bad", "ip.rx_csum_bad"], ">0")];
        let fails = evaluate_disjunctive(&pre, &post, exp);
        assert!(fails.is_empty(), "expected pass, got {fails:?}");
    }

    #[test]
    fn disjunctive_fails_when_no_counter_fires() {
        let pre = snap(&[("eth.rx_drop_cksum_bad", 0), ("ip.rx_csum_bad", 0)]);
        let post = snap(&[("eth.rx_drop_cksum_bad", 0), ("ip.rx_csum_bad", 0)]);
        let exp: &[(&[&str], &str)] =
            &[(&["eth.rx_drop_cksum_bad", "ip.rx_csum_bad"], ">0")];
        let fails = evaluate_disjunctive(&pre, &post, exp);
        assert_eq!(fails.len(), 1);
        match &fails[0] {
            FailureReason::DisjunctiveCounterRelation { counters, .. } => {
                assert_eq!(counters.len(), 2);
            }
            other => panic!("expected DisjunctiveCounterRelation, got {other:?}"),
        }
    }

    #[test]
    fn global_side_checks_pass_when_both_zero() {
        let pre = snap(&[("tcp.mbuf_refcnt_drop_unexpected", 0), ("obs.events_dropped", 0)]);
        let post = pre.clone();
        let fails = evaluate_global_side_checks(&pre, &post);
        assert!(fails.is_empty());
    }

    #[test]
    fn global_side_checks_fail_when_mbuf_refcnt_drop_nonzero() {
        let pre = snap(&[("tcp.mbuf_refcnt_drop_unexpected", 0), ("obs.events_dropped", 0)]);
        let post = snap(&[("tcp.mbuf_refcnt_drop_unexpected", 3), ("obs.events_dropped", 0)]);
        let fails = evaluate_global_side_checks(&pre, &post);
        assert_eq!(fails.len(), 1);
    }
}
