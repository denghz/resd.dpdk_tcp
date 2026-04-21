//! The unified CSV row emitted by every bench tool. Spec §14.
//!
//! Layout: the run-invariant fields from `RunMetadata` come first, followed by
//! the per-row fields `tool`, `test_case`, `feature_set`, `dimensions_json`,
//! `metric_name`, `metric_unit`, `metric_value`, `metric_aggregation`.
//!
//! # Why a hand-written `Serialize`/`Deserialize`
//!
//! `csv::Writer` only supports struct serialisation via `SerializeStruct` — it
//! rejects `SerializeMap`, which is what both plain `#[serde(flatten)]` and
//! custom flatten-helper modules (`with = "..."`) compile into under the
//! hood. The design sketch in the plan warned about this exact trade-off and
//! explicitly authorised the fallback: flatten manually.
//!
//! The approach here:
//!
//! - Keep `RunMetadata` and `Preconditions` as ergonomic nested structs for
//!   callers populating a row — they stay strongly typed and easy to build.
//! - Implement `Serialize` on `CsvRow` by hand, writing one `SerializeStruct`
//!   with all 37 (= 13 run-metadata + 14 precondition + 2 mode + 8 per-row)
//!   columns as flat scalar fields. That shape is exactly what csv expects.
//! - Implement `Deserialize` on `CsvRow` via a `Visitor` that walks the
//!   matching set of field keys and rebuilds the nested structs.
//!
//! `metric_value` is a raw `f64`. Values chosen in the round-trip test are
//! exactly representable so bit-exact `PartialEq` compares work. Real runs
//! never compare values for equality, only aggregate.

use serde::de::{MapAccess, Visitor};
use serde::ser::SerializeStruct;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

use crate::preconditions::{PreconditionMode, Preconditions};
use crate::run_metadata::RunMetadata;

pub use crate::preconditions::PreconditionValue;

/// Aggregation bucket a `metric_value` represents. Spec §14.2.
///
/// No `raw` variant — raw samples live in sidecar files (spec §14.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricAggregation {
    P50,
    P99,
    P999,
    Mean,
    Stddev,
    Ci95Lower,
    Ci95Upper,
}

impl fmt::Display for MetricAggregation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::P50 => write!(f, "p50"),
            Self::P99 => write!(f, "p99"),
            Self::P999 => write!(f, "p999"),
            Self::Mean => write!(f, "mean"),
            Self::Stddev => write!(f, "stddev"),
            Self::Ci95Lower => write!(f, "ci95_lower"),
            Self::Ci95Upper => write!(f, "ci95_upper"),
        }
    }
}

/// The full list of CSV columns in spec order (§14.1). Kept as a module-level
/// constant so the `Serialize` impl and the `Deserialize` visitor use exactly
/// the same ordering — if a column is added / removed it only changes here.
const COLUMNS: &[&str] = &[
    // Run-invariant columns
    "run_id",
    "run_started_at",
    "commit_sha",
    "branch",
    "host",
    "instance_type",
    "cpu_model",
    "dpdk_version",
    "kernel",
    "nic_model",
    "nic_fw",
    "ami_id",
    "precondition_mode",
    // 14 precondition columns
    "precondition_isolcpus",
    "precondition_nohz_full",
    "precondition_rcu_nocbs",
    "precondition_governor",
    "precondition_cstate_max",
    "precondition_tsc_invariant",
    "precondition_coalesce_off",
    "precondition_tso_off",
    "precondition_lro_off",
    "precondition_rss_on",
    "precondition_thermal_throttle",
    "precondition_hugepages_reserved",
    "precondition_irqbalance_off",
    "precondition_wc_active",
    // Per-row columns
    "tool",
    "test_case",
    "feature_set",
    "dimensions_json",
    "metric_name",
    "metric_unit",
    "metric_value",
    "metric_aggregation",
];

/// One row in the unified benchmark CSV. Spec §14.1.
#[derive(Debug, Clone, PartialEq)]
pub struct CsvRow {
    pub run_metadata: RunMetadata,
    pub tool: String,
    pub test_case: String,
    pub feature_set: String,
    pub dimensions_json: String,
    pub metric_name: String,
    pub metric_unit: String,
    pub metric_value: f64,
    pub metric_aggregation: MetricAggregation,
}

impl CsvRow {
    /// Serialise this row into a `csv::Writer` and flush. If the writer has
    /// no records yet, csv emits the header automatically on the first call.
    pub fn write_with_header<W: std::io::Write>(
        &self,
        wtr: &mut csv::Writer<W>,
    ) -> Result<(), csv::Error> {
        wtr.serialize(self)?;
        wtr.flush()?;
        Ok(())
    }
}

// -- Serialize ---------------------------------------------------------------

impl Serialize for CsvRow {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        let m = &self.run_metadata;
        let p = &m.preconditions;
        let mut st = s.serialize_struct("CsvRow", COLUMNS.len())?;
        st.serialize_field("run_id", &m.run_id)?;
        st.serialize_field("run_started_at", &m.run_started_at)?;
        st.serialize_field("commit_sha", &m.commit_sha)?;
        st.serialize_field("branch", &m.branch)?;
        st.serialize_field("host", &m.host)?;
        st.serialize_field("instance_type", &m.instance_type)?;
        st.serialize_field("cpu_model", &m.cpu_model)?;
        st.serialize_field("dpdk_version", &m.dpdk_version)?;
        st.serialize_field("kernel", &m.kernel)?;
        st.serialize_field("nic_model", &m.nic_model)?;
        st.serialize_field("nic_fw", &m.nic_fw)?;
        st.serialize_field("ami_id", &m.ami_id)?;
        st.serialize_field("precondition_mode", &m.precondition_mode)?;
        st.serialize_field("precondition_isolcpus", &p.isolcpus)?;
        st.serialize_field("precondition_nohz_full", &p.nohz_full)?;
        st.serialize_field("precondition_rcu_nocbs", &p.rcu_nocbs)?;
        st.serialize_field("precondition_governor", &p.governor)?;
        st.serialize_field("precondition_cstate_max", &p.cstate_max)?;
        st.serialize_field("precondition_tsc_invariant", &p.tsc_invariant)?;
        st.serialize_field("precondition_coalesce_off", &p.coalesce_off)?;
        st.serialize_field("precondition_tso_off", &p.tso_off)?;
        st.serialize_field("precondition_lro_off", &p.lro_off)?;
        st.serialize_field("precondition_rss_on", &p.rss_on)?;
        st.serialize_field("precondition_thermal_throttle", &p.thermal_throttle)?;
        st.serialize_field("precondition_hugepages_reserved", &p.hugepages_reserved)?;
        st.serialize_field("precondition_irqbalance_off", &p.irqbalance_off)?;
        st.serialize_field("precondition_wc_active", &p.wc_active)?;
        st.serialize_field("tool", &self.tool)?;
        st.serialize_field("test_case", &self.test_case)?;
        st.serialize_field("feature_set", &self.feature_set)?;
        st.serialize_field("dimensions_json", &self.dimensions_json)?;
        st.serialize_field("metric_name", &self.metric_name)?;
        st.serialize_field("metric_unit", &self.metric_unit)?;
        st.serialize_field("metric_value", &self.metric_value)?;
        st.serialize_field("metric_aggregation", &self.metric_aggregation)?;
        st.end()
    }
}

// -- Deserialize -------------------------------------------------------------

impl<'de> Deserialize<'de> for CsvRow {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_struct("CsvRow", COLUMNS, CsvRowVisitor)
    }
}

struct CsvRowVisitor;

impl<'de> Visitor<'de> for CsvRowVisitor {
    type Value = CsvRow;

    fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("a CsvRow record with the spec §14 column set")
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Self::Value, A::Error> {
        // Optional field buffers — we build each up as we see the matching key.
        let mut run_id: Option<uuid::Uuid> = None;
        let mut run_started_at: Option<String> = None;
        let mut commit_sha: Option<String> = None;
        let mut branch: Option<String> = None;
        let mut host: Option<String> = None;
        let mut instance_type: Option<String> = None;
        let mut cpu_model: Option<String> = None;
        let mut dpdk_version: Option<String> = None;
        let mut kernel: Option<String> = None;
        let mut nic_model: Option<String> = None;
        let mut nic_fw: Option<String> = None;
        let mut ami_id: Option<String> = None;
        let mut precondition_mode: Option<PreconditionMode> = None;

        let mut p = Preconditions::default();

        let mut tool: Option<String> = None;
        let mut test_case: Option<String> = None;
        let mut feature_set: Option<String> = None;
        let mut dimensions_json: Option<String> = None;
        let mut metric_name: Option<String> = None;
        let mut metric_unit: Option<String> = None;
        let mut metric_value: Option<f64> = None;
        let mut metric_aggregation: Option<MetricAggregation> = None;

        while let Some(key) = map.next_key::<String>()? {
            match key.as_str() {
                "run_id" => run_id = Some(map.next_value()?),
                "run_started_at" => run_started_at = Some(map.next_value()?),
                "commit_sha" => commit_sha = Some(map.next_value()?),
                "branch" => branch = Some(map.next_value()?),
                "host" => host = Some(map.next_value()?),
                "instance_type" => instance_type = Some(map.next_value()?),
                "cpu_model" => cpu_model = Some(map.next_value()?),
                "dpdk_version" => dpdk_version = Some(map.next_value()?),
                "kernel" => kernel = Some(map.next_value()?),
                "nic_model" => nic_model = Some(map.next_value()?),
                "nic_fw" => nic_fw = Some(map.next_value()?),
                "ami_id" => ami_id = Some(map.next_value()?),
                "precondition_mode" => precondition_mode = Some(map.next_value()?),
                "precondition_isolcpus" => {
                    p.isolcpus = map.next_value::<PreconditionValue>()?;
                }
                "precondition_nohz_full" => p.nohz_full = map.next_value()?,
                "precondition_rcu_nocbs" => p.rcu_nocbs = map.next_value()?,
                "precondition_governor" => p.governor = map.next_value()?,
                "precondition_cstate_max" => p.cstate_max = map.next_value()?,
                "precondition_tsc_invariant" => p.tsc_invariant = map.next_value()?,
                "precondition_coalesce_off" => p.coalesce_off = map.next_value()?,
                "precondition_tso_off" => p.tso_off = map.next_value()?,
                "precondition_lro_off" => p.lro_off = map.next_value()?,
                "precondition_rss_on" => p.rss_on = map.next_value()?,
                "precondition_thermal_throttle" => p.thermal_throttle = map.next_value()?,
                "precondition_hugepages_reserved" => p.hugepages_reserved = map.next_value()?,
                "precondition_irqbalance_off" => p.irqbalance_off = map.next_value()?,
                "precondition_wc_active" => p.wc_active = map.next_value()?,
                "tool" => tool = Some(map.next_value()?),
                "test_case" => test_case = Some(map.next_value()?),
                "feature_set" => feature_set = Some(map.next_value()?),
                "dimensions_json" => dimensions_json = Some(map.next_value()?),
                "metric_name" => metric_name = Some(map.next_value()?),
                "metric_unit" => metric_unit = Some(map.next_value()?),
                "metric_value" => metric_value = Some(map.next_value()?),
                "metric_aggregation" => metric_aggregation = Some(map.next_value()?),
                _ => {
                    // Unknown column — skip. Forward-compat with future
                    // schema additions that older tools don't know about.
                    let _: serde::de::IgnoredAny = map.next_value()?;
                }
            }
        }

        fn require<T, E: serde::de::Error>(v: Option<T>, name: &'static str) -> Result<T, E> {
            v.ok_or_else(|| E::missing_field(name))
        }

        let run_metadata = RunMetadata {
            run_id: require(run_id, "run_id")?,
            run_started_at: require(run_started_at, "run_started_at")?,
            commit_sha: require(commit_sha, "commit_sha")?,
            branch: require(branch, "branch")?,
            host: require(host, "host")?,
            instance_type: require(instance_type, "instance_type")?,
            cpu_model: require(cpu_model, "cpu_model")?,
            dpdk_version: require(dpdk_version, "dpdk_version")?,
            kernel: require(kernel, "kernel")?,
            nic_model: require(nic_model, "nic_model")?,
            nic_fw: require(nic_fw, "nic_fw")?,
            ami_id: require(ami_id, "ami_id")?,
            precondition_mode: require(precondition_mode, "precondition_mode")?,
            preconditions: p,
        };

        Ok(CsvRow {
            run_metadata,
            tool: require(tool, "tool")?,
            test_case: require(test_case, "test_case")?,
            feature_set: require(feature_set, "feature_set")?,
            dimensions_json: require(dimensions_json, "dimensions_json")?,
            metric_name: require(metric_name, "metric_name")?,
            metric_unit: require(metric_unit, "metric_unit")?,
            metric_value: require(metric_value, "metric_value")?,
            metric_aggregation: require(metric_aggregation, "metric_aggregation")?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metric_aggregation_display_matches_serde() {
        for v in [
            MetricAggregation::P50,
            MetricAggregation::P99,
            MetricAggregation::P999,
            MetricAggregation::Mean,
            MetricAggregation::Stddev,
            MetricAggregation::Ci95Lower,
            MetricAggregation::Ci95Upper,
        ] {
            let disp = v.to_string();
            let ser = serde_json::to_string(&v).unwrap();
            assert_eq!(ser, format!("\"{}\"", disp));
        }
    }
}
