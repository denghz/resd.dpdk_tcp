//! Pressure-test failure-bundle helper + counter-snapshot DSL (A11.0 step 3 / T2).
//!
//! Centralizes the two primitives every pressure-test suite re-uses:
//!
//!   1. **Counter snapshots.** [`CounterSnapshot::capture`] reads every name
//!      in `ALL_COUNTER_NAMES` (delta `AtomicU64`s) plus the two known
//!      level counters reachable via `Counters::read_level_counter_u32`
//!      (`tcp.tx_data_mempool_avail`, `tcp.rx_mempool_avail`) into a
//!      `BTreeMap`. [`CounterSnapshot::delta_since`] returns a signed
//!      [`CounterDelta`] (level counters can drop, so deltas are `i64`).
//!      [`assert_delta`] applies a [`Relation`] to a named delta and panics
//!      with a structured `actual=… expected=…` message on mismatch.
//!
//!   2. **Failure bundles.** [`dump_failure_bundle`] writes a forensic
//!      directory under `target/pressure-test/<suite>/<bucket>/<timestamp>/`
//!      containing JSON snapshots of every counter (before / after / delta),
//!      a `Debug`-rendered `EngineConfig`, the most recent up-to-1024
//!      `InternalEvent`s, and the error string. Suites call this from a
//!      `catch_unwind` block before re-raising so a CI failure leaves
//!      everything needed to re-create the wedge on disk.
//!
//! Per project memory `feedback_observability_primitives_only.md`: the
//! library exposes counters / events; the *application* (this helper)
//! aggregates and routes them. The helper therefore does its own
//! serialization and never asks the library to dump itself.
//!
//! No serde dependency is taken — counters are plain `u64`/`i64` maps that
//! we hand-format as JSON with `BTreeMap` ordering for stable diffs;
//! events + config use `Debug` so format changes do not require the helper
//! to be regenerated.
//!
//! Gated behind the `pressure-test` cargo feature; the surrounding
//! `tests/common/mod.rs` declares the module conditionally so non-pressure
//! integration test binaries see no extra code.

#![cfg(feature = "pressure-test")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use dpdk_net_core::counters::{lookup_counter, Counters, ALL_COUNTER_NAMES};
use dpdk_net_core::engine::EngineConfig;
use dpdk_net_core::tcp_events::InternalEvent;

/// Names of the level counters (`AtomicU32`, last-sampled value) accessible
/// via `Counters::read_level_counter_u32`. Hard-coded because the level
/// counter set is very small (two entries) and the access function only
/// resolves these two paths.
const LEVEL_COUNTER_NAMES: &[&str] = &["tcp.tx_data_mempool_avail", "tcp.rx_mempool_avail"];

/// A point-in-time copy of every declared counter on a `Counters` instance.
///
/// `delta` covers the `AtomicU64` group (`ALL_COUNTER_NAMES`); `level_u32`
/// covers the small set of `AtomicU32` last-sampled level counters. The two
/// groups are kept separate because their delta semantics differ — delta
/// counters monotonically increase between `capture` calls, level counters
/// can move in either direction.
#[derive(Debug, Clone)]
pub struct CounterSnapshot {
    /// `name -> value` for every `ALL_COUNTER_NAMES` entry.
    pub delta: BTreeMap<String, u64>,
    /// `name -> value` for every level (`AtomicU32`) counter.
    pub level_u32: BTreeMap<String, u32>,
}

impl CounterSnapshot {
    /// Capture every declared counter on `c`. Reads use `Ordering::Relaxed`
    /// — slow-path consumers accept that they may see a counter's value
    /// staggered relative to other counters bumped on the same iteration.
    pub fn capture(c: &Counters) -> Self {
        let mut delta = BTreeMap::new();
        for name in ALL_COUNTER_NAMES {
            let v = lookup_counter(c, name)
                .map(|a| a.load(Ordering::Relaxed))
                .unwrap_or(0);
            delta.insert((*name).to_string(), v);
        }
        let mut level_u32 = BTreeMap::new();
        for name in LEVEL_COUNTER_NAMES {
            if let Some(v) = c.read_level_counter_u32(name) {
                level_u32.insert((*name).to_string(), v);
            }
        }
        Self { delta, level_u32 }
    }

    /// Compute `self - before` per counter. Delta counters use signed `i64`
    /// (saturated to handle the impossible-but-not-prohibited "counter
    /// went backwards" case without panicking on subtraction overflow);
    /// level counters use signed `i64` for the same reason but with `i64`
    /// always (their natural range is `u32` so any subtraction fits).
    pub fn delta_since(&self, before: &Self) -> CounterDelta {
        let mut delta = BTreeMap::new();
        for (name, after_v) in &self.delta {
            let before_v = before.delta.get(name).copied().unwrap_or(0);
            let signed = (*after_v as i128) - (before_v as i128);
            // Saturate: pressure tests assert deltas in the i64 range.
            let signed = signed.max(i64::MIN as i128).min(i64::MAX as i128) as i64;
            delta.insert(name.clone(), signed);
        }
        let mut level_u32 = BTreeMap::new();
        for (name, after_v) in &self.level_u32 {
            let before_v = before.level_u32.get(name).copied().unwrap_or(0);
            let signed = (*after_v as i64) - (before_v as i64);
            level_u32.insert(name.clone(), signed);
        }
        CounterDelta { delta, level_u32 }
    }
}

/// Signed deltas between two `CounterSnapshot`s. Positive = went up;
/// negative = went down (only meaningful for level counters under normal
/// operation). Names mirror `CounterSnapshot`.
#[derive(Debug, Clone)]
pub struct CounterDelta {
    /// Signed delta for each `ALL_COUNTER_NAMES` entry.
    pub delta: BTreeMap<String, i64>,
    /// Signed delta for each level counter.
    pub level_u32: BTreeMap<String, i64>,
}

/// Comparison the suite wants to enforce on a counter's delta. Each variant
/// carries its operand(s) inline; assertion failure renders the variant in
/// the panic message so the failure log identifies the violated rule.
#[derive(Debug, Clone, Copy)]
pub enum Relation {
    /// `actual == n`.
    Eq(i64),
    /// `actual > n`.
    Gt(i64),
    /// `actual >= n`.
    Ge(i64),
    /// `actual <= n`.
    Le(i64),
    /// `lo <= actual <= hi`. Both endpoints inclusive.
    Range(i64, i64),
}

/// Apply `rel` to `d.delta[name]` and panic with a structured message on
/// failure. Falls back to `d.level_u32[name]` if the name is not in
/// `d.delta` (so suites can write `assert_delta(&d, "tcp.tx_data_mempool_avail", …)`
/// without distinguishing groups). Panics if `name` is in neither map.
///
/// The panic message format is intentionally regular (`name=… actual=… expected=…`)
/// so both humans and shell-grep-driven failure aggregators can parse it.
pub fn assert_delta(d: &CounterDelta, name: &str, rel: Relation) {
    let actual = d
        .delta
        .get(name)
        .copied()
        .or_else(|| d.level_u32.get(name).copied())
        .unwrap_or_else(|| {
            panic!(
                "assert_delta: counter `{name}` is in neither delta nor level_u32 maps"
            )
        });
    let ok = match rel {
        Relation::Eq(n) => actual == n,
        Relation::Gt(n) => actual > n,
        Relation::Ge(n) => actual >= n,
        Relation::Le(n) => actual <= n,
        Relation::Range(lo, hi) => actual >= lo && actual <= hi,
    };
    if !ok {
        panic!(
            "assert_delta failed: name=`{name}` actual={actual} expected={rel:?}"
        );
    }
}

/// Forensic snapshot a suite hands to [`dump_failure_bundle`] when an
/// assertion fails. References-only — the helper does no cloning of the
/// large blobs (counters, config, events) before serialization.
pub struct FailureCtx<'a> {
    /// Snapshot taken before the bucket's work started.
    pub before: &'a CounterSnapshot,
    /// Snapshot taken after the failure was detected.
    pub after: &'a CounterSnapshot,
    /// `EngineConfig` the engine was constructed with. Dumped via `Debug`
    /// (no serde dep on the helper).
    pub config: &'a EngineConfig,
    /// Up to the most-recent 1024 events drained from the engine's queue.
    /// Suites populate this by repeatedly calling `Engine::events().pop()`
    /// before invoking the helper. Empty is acceptable — engines that
    /// never emitted an event still get a non-empty bundle.
    pub events: Vec<InternalEvent>,
    /// Free-form error string. Suites typically pass the panic message
    /// captured via `catch_unwind` here.
    pub last_error: String,
}

/// Process-wide override for the bundle root, settable from a test via
/// [`set_bundle_root`]. The first writer wins (`OnceLock::set`); test code
/// that needs an isolated root must call `set_bundle_root` before any
/// `dump_failure_bundle` invocation. CI runs that consume bundles via the
/// default `target/pressure-test/` layout never set this.
static BUNDLE_ROOT_OVERRIDE: OnceLock<PathBuf> = OnceLock::new();

/// Pin the bundle root to `path` for the lifetime of the process. Used by
/// the smoke test to redirect bundles into `CARGO_TARGET_TMPDIR` so the
/// cleanup `remove_dir_all` cannot accidentally walk into a developer's
/// real `target/pressure-test/`. Subsequent calls are no-ops (the OnceLock
/// is set-once); the smoke test must pick a fresh path on the first call.
pub fn set_bundle_root(path: PathBuf) {
    let _ = BUNDLE_ROOT_OVERRIDE.set(path);
}

/// Resolve the pressure-test bundle root. Order of precedence:
///   1. Process-wide override pinned by [`set_bundle_root`].
///   2. `DPDK_NET_PRESSURE_TEST_DIR` env var (CI override).
///   3. `target/pressure-test/` relative to a `target/` sibling found by
///      walking up from the current working directory.
fn bundle_root() -> PathBuf {
    if let Some(p) = BUNDLE_ROOT_OVERRIDE.get() {
        return p.clone();
    }
    if let Ok(p) = std::env::var("DPDK_NET_PRESSURE_TEST_DIR") {
        return PathBuf::from(p);
    }
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut probe: Option<PathBuf> = Some(cwd.clone());
    while let Some(p) = probe {
        let candidate = p.join("target");
        if candidate.is_dir() {
            return candidate.join("pressure-test");
        }
        probe = p.parent().map(Path::to_path_buf);
    }
    cwd.join("target").join("pressure-test")
}

/// Write a failure bundle to
/// `target/pressure-test/<suite>/<bucket>/<unix_ms>/` and return the path.
///
/// Files written:
///   * `counters_before.json` — JSON object `{ "delta": { … }, "level_u32": { … } }`.
///   * `counters_after.json`  — same shape.
///   * `counters_delta.json`  — same shape, signed values.
///   * `config.txt`           — `Debug`-rendered `EngineConfig`.
///   * `events.log`           — one `Debug`-rendered `InternalEvent` per line.
///   * `error.txt`            — `last_error` verbatim.
///
/// Returns the absolute path of the per-timestamp directory. Errors during
/// write are best-effort: a partial bundle is preferable to no bundle at all
/// when a CI failure already has the suite mid-panic. Use
/// `DPDK_NET_PRESSURE_TEST_DIR` to redirect the root in tests.
pub fn dump_failure_bundle(suite: &str, bucket: &str, ctx: &FailureCtx) -> PathBuf {
    let unix_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dir = bundle_root()
        .join(suite)
        .join(bucket)
        .join(format!("{unix_ms}"));
    let _ = std::fs::create_dir_all(&dir);

    let _ = std::fs::write(
        dir.join("counters_before.json"),
        snapshot_to_json(ctx.before),
    );
    let _ = std::fs::write(
        dir.join("counters_after.json"),
        snapshot_to_json(ctx.after),
    );
    let delta = ctx.after.delta_since(ctx.before);
    let _ = std::fs::write(dir.join("counters_delta.json"), delta_to_json(&delta));

    let _ = std::fs::write(dir.join("config.txt"), format!("{:#?}\n", ctx.config));

    let mut events = String::new();
    let take_n = ctx.events.len().min(1024);
    let start = ctx.events.len().saturating_sub(take_n);
    for ev in &ctx.events[start..] {
        events.push_str(&format!("{ev:?}\n"));
    }
    let _ = std::fs::write(dir.join("events.log"), events);

    let _ = std::fs::write(dir.join("error.txt"), &ctx.last_error);

    dir
}

/// Serialize a `CounterSnapshot` to JSON without pulling in serde. Output is
/// deterministic (BTreeMap iteration order) so a `diff` between bundles is
/// meaningful.
fn snapshot_to_json(s: &CounterSnapshot) -> String {
    let mut out = String::new();
    out.push_str("{\n  \"delta\": {");
    let mut first = true;
    for (k, v) in &s.delta {
        if first {
            first = false;
            out.push('\n');
        } else {
            out.push_str(",\n");
        }
        out.push_str(&format!("    \"{}\": {}", json_escape(k), v));
    }
    out.push_str("\n  },\n  \"level_u32\": {");
    let mut first = true;
    for (k, v) in &s.level_u32 {
        if first {
            first = false;
            out.push('\n');
        } else {
            out.push_str(",\n");
        }
        out.push_str(&format!("    \"{}\": {}", json_escape(k), v));
    }
    out.push_str("\n  }\n}\n");
    out
}

/// Serialize a `CounterDelta` to JSON. Same shape as `snapshot_to_json` but
/// the values are signed.
fn delta_to_json(d: &CounterDelta) -> String {
    let mut out = String::new();
    out.push_str("{\n  \"delta\": {");
    let mut first = true;
    for (k, v) in &d.delta {
        if first {
            first = false;
            out.push('\n');
        } else {
            out.push_str(",\n");
        }
        out.push_str(&format!("    \"{}\": {}", json_escape(k), v));
    }
    out.push_str("\n  },\n  \"level_u32\": {");
    let mut first = true;
    for (k, v) in &d.level_u32 {
        if first {
            first = false;
            out.push('\n');
        } else {
            out.push_str(",\n");
        }
        out.push_str(&format!("    \"{}\": {}", json_escape(k), v));
    }
    out.push_str("\n  }\n}\n");
    out
}

/// Minimal JSON string escape — counter names contain only `[a-z0-9_.]`,
/// so we only need to handle the bare-bones cases. `\"` and `\\` are
/// covered defensively in case a future counter name contains them.
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

/// Suite-side ergonomic wrapper. A pressure-test suite constructs one
/// `PressureBucket` per labeled scenario it executes, calls
/// `before` snapshot via the helper at construction, then calls
/// [`PressureBucket::finish_ok`] on success or [`PressureBucket::finish_fail`]
/// on a caught panic to dump the bundle and re-raise.
///
/// The wrapper deliberately keeps the suite in charge of the engine handle
/// (so the suite can decide which engine accessor to read events from) and
/// only owns the snapshot + label.
pub struct PressureBucket {
    /// Suite name (top-level subdirectory under `target/pressure-test/`).
    pub suite: String,
    /// Bucket label (second-level subdirectory; usually the test-case name).
    pub bucket: String,
    /// Snapshot captured at bucket entry.
    pub before: CounterSnapshot,
}

impl PressureBucket {
    /// Capture the entry-time snapshot and return a fresh bucket. The suite
    /// decides the bucket name; convention is `<scenario>_<config-hash>` so
    /// repeated runs of the same scenario under different configs do not
    /// collide on disk.
    pub fn open(suite: &str, bucket: &str, counters: &Counters) -> Self {
        Self {
            suite: suite.to_string(),
            bucket: bucket.to_string(),
            before: CounterSnapshot::capture(counters),
        }
    }

    /// Called on success — drops the bucket without writing anything.
    pub fn finish_ok(self) {
        // No-op: the snapshot is forgotten. Kept as a method so suites have
        // a symmetric API (`finish_ok` / `finish_fail`) and a place to add
        // success-path bookkeeping later (e.g. metric emission) without
        // touching every call site.
    }

    /// Called on failure — captures an `after` snapshot, dumps the bundle,
    /// and returns its path so the suite can include it in the panic
    /// message it re-raises.
    pub fn finish_fail(
        self,
        counters: &Counters,
        config: &EngineConfig,
        events: Vec<InternalEvent>,
        last_error: String,
    ) -> PathBuf {
        let after = CounterSnapshot::capture(counters);
        let ctx = FailureCtx {
            before: &self.before,
            after: &after,
            config,
            events,
            last_error,
        };
        dump_failure_bundle(&self.suite, &self.bucket, &ctx)
    }
}
