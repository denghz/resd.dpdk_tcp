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
