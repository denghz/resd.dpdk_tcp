//! Precondition-check data plumbing. Spec §4.1 + §4.3.
//!
//! Each host-level precondition resolves to a `PreconditionValue` which encodes
//! both the pass/fail verdict and an optional observed-value string (e.g.
//! `pass=2-7` for an `isolcpus` mask, `fail=C6` for a wrong c-state). The
//! string form `"pass"` / `"fail"` / `"pass=X"` / `"fail=X"` is what we emit
//! into the CSV and what we parse back when a downstream tool (bench-report)
//! re-reads the file.

use serde::{de::Error as _, Deserialize, Deserializer, Serialize, Serializer};
use std::str::FromStr;

/// Enforcement regime for precondition checks. Spec §4.3.
///
/// `Strict`: any fail aborts the run; `Lenient`: failures are recorded but the
/// run continues (rows are still flagged via the per-precondition columns).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PreconditionMode {
    Strict,
    Lenient,
}

impl Default for PreconditionMode {
    fn default() -> Self {
        Self::Strict
    }
}

impl std::fmt::Display for PreconditionMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Strict => write!(f, "strict"),
            Self::Lenient => write!(f, "lenient"),
        }
    }
}

impl FromStr for PreconditionMode {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "strict" => Ok(Self::Strict),
            "lenient" => Ok(Self::Lenient),
            other => Err(format!("unknown precondition mode: {other}")),
        }
    }
}

/// A single precondition result — verdict plus an optional observed value
/// (e.g. `isolcpus=2-7` or `cstate=C6`). Spec §4.3.
///
/// Serde impl is hand-written so the value appears as a single CSV cell
/// (`"pass=2-7"`, `"fail=C6"`, `"pass"`, `"fail"`) rather than as two
/// sub-columns. This is what csv::Writer expects from each struct field.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PreconditionValue {
    pub passed: bool,
    pub value: String,
}

impl PreconditionValue {
    pub fn pass() -> Self {
        Self {
            passed: true,
            value: String::new(),
        }
    }

    pub fn fail() -> Self {
        Self {
            passed: false,
            value: String::new(),
        }
    }

    pub fn pass_with(value: impl Into<String>) -> Self {
        Self {
            passed: true,
            value: value.into(),
        }
    }

    pub fn fail_with(value: impl Into<String>) -> Self {
        Self {
            passed: false,
            value: value.into(),
        }
    }
}

impl FromStr for PreconditionValue {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s == "pass" {
            return Ok(Self {
                passed: true,
                value: String::new(),
            });
        }
        if s == "fail" {
            return Ok(Self {
                passed: false,
                value: String::new(),
            });
        }
        if let Some(rest) = s.strip_prefix("pass=") {
            return Ok(Self {
                passed: true,
                value: rest.into(),
            });
        }
        if let Some(rest) = s.strip_prefix("fail=") {
            return Ok(Self {
                passed: false,
                value: rest.into(),
            });
        }
        Err(format!("unparseable precondition value: {s}"))
    }
}

impl std::fmt::Display for PreconditionValue {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.value.is_empty() {
            write!(f, "{}", if self.passed { "pass" } else { "fail" })
        } else {
            write!(
                f,
                "{}={}",
                if self.passed { "pass" } else { "fail" },
                self.value
            )
        }
    }
}

impl Serialize for PreconditionValue {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PreconditionValue {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        s.parse().map_err(D::Error::custom)
    }
}

/// The full set of 14 host-level preconditions checked at run start. Spec §4.1.
///
/// Each field carries the `precondition_*` column-name prefix defined in
/// spec §14.1 via `#[serde(rename = "...")]`, so when this struct is flattened
/// into `RunMetadata` via `#[serde(flatten)]` the CSV header comes out with
/// exactly the schema columns. (We pay the bookkeeping cost here instead of
/// in a custom flatten module because `csv::Writer` does not support the
/// `serialize_map` pathway that a `with = "..."` flatten helper would need.)
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preconditions {
    #[serde(rename = "precondition_isolcpus")]
    pub isolcpus: PreconditionValue,
    #[serde(rename = "precondition_nohz_full")]
    pub nohz_full: PreconditionValue,
    #[serde(rename = "precondition_rcu_nocbs")]
    pub rcu_nocbs: PreconditionValue,
    #[serde(rename = "precondition_governor")]
    pub governor: PreconditionValue,
    #[serde(rename = "precondition_cstate_max")]
    pub cstate_max: PreconditionValue,
    #[serde(rename = "precondition_tsc_invariant")]
    pub tsc_invariant: PreconditionValue,
    #[serde(rename = "precondition_coalesce_off")]
    pub coalesce_off: PreconditionValue,
    #[serde(rename = "precondition_tso_off")]
    pub tso_off: PreconditionValue,
    #[serde(rename = "precondition_lro_off")]
    pub lro_off: PreconditionValue,
    #[serde(rename = "precondition_rss_on")]
    pub rss_on: PreconditionValue,
    #[serde(rename = "precondition_thermal_throttle")]
    pub thermal_throttle: PreconditionValue,
    #[serde(rename = "precondition_hugepages_reserved")]
    pub hugepages_reserved: PreconditionValue,
    #[serde(rename = "precondition_irqbalance_off")]
    pub irqbalance_off: PreconditionValue,
    #[serde(rename = "precondition_wc_active")]
    pub wc_active: PreconditionValue,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_display_round_trips() {
        for (s, expect) in [
            ("pass", PreconditionValue::pass()),
            ("fail", PreconditionValue::fail()),
            ("pass=2-7", PreconditionValue::pass_with("2-7")),
            ("fail=C6", PreconditionValue::fail_with("C6")),
        ] {
            let parsed: PreconditionValue = s.parse().unwrap();
            assert_eq!(parsed, expect);
            assert_eq!(parsed.to_string(), s);
        }
    }

    #[test]
    fn value_rejects_garbage() {
        let err = "unknown".parse::<PreconditionValue>();
        assert!(err.is_err());
    }

    #[test]
    fn mode_round_trips() {
        assert_eq!("strict".parse::<PreconditionMode>().unwrap(), PreconditionMode::Strict);
        assert_eq!("lenient".parse::<PreconditionMode>().unwrap(), PreconditionMode::Lenient);
        assert_eq!(PreconditionMode::Strict.to_string(), "strict");
        assert_eq!(PreconditionMode::Lenient.to_string(), "lenient");
    }

    #[test]
    fn value_serde_is_single_string() {
        let v = PreconditionValue::pass_with("2-7");
        let json = serde_json::to_string(&v).unwrap();
        assert_eq!(json, "\"pass=2-7\"");
        let back: PreconditionValue = serde_json::from_str("\"pass=2-7\"").unwrap();
        assert_eq!(back, v);
    }
}
