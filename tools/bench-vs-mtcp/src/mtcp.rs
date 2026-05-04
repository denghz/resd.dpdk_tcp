//! mTCP stack driver — subprocess wrapper around the C-side mtcp-driver
//! binary.
//!
//! # Architecture
//!
//! mTCP only compiles against DPDK 20.11 (mTCP upstream's last
//! ABI-compatible DPDK; see image-builder component
//! `04-install-mtcp.yaml` for the sidecar install). bench-vs-mtcp's
//! Rust process already statically links DPDK 23.11 via
//! `dpdk-net-sys`, and DPDK doesn't support side-by-side major-version
//! linkage in the same process (same EAL symbols, different ABI). So
//! the mTCP arm runs as a **subprocess**: a separate C binary
//! (`/opt/mtcp-peer/mtcp-driver`) links libmtcp.a + DPDK 20.11 cleanly,
//! receives workload params on its CLI, and returns a JSON result on
//! stdout that this module parses back into bench-common samples.
//!
//! # AMI layout (per `04-install-mtcp.yaml`)
//!
//! - `/usr/local/dpdk-20.11/` — sidecar DPDK install
//! - `/opt/mtcp/lib/libmtcp.a` — built mTCP archive
//! - `/opt/mtcp/include/mtcp_api.h` — public mTCP API
//! - `/opt/mtcp-peer/bench-peer` — server-side echo binary (peer host)
//! - `/opt/mtcp-peer/mtcp-driver` — client-side workload driver (this
//!   host); built from `tools/bench-vs-mtcp/peer/mtcp-driver.c`
//!
//! # Status
//!
//! - **DPDK 20.11 sidecar:** built + installed via image-builder.
//! - **libmtcp.a:** built against DPDK 20.11 with 3 patches (Makefile
//!   pkg-config + `-fcommon` + `lcore_config[]` shim).
//! - **bench-peer (server):** built — multi-core mTCP echo loop.
//! - **mtcp-driver (client):** **STUB** — parses CLI, returns ENOSYS
//!   so this wrapper surfaces a clear "driver not yet implemented"
//!   error. Full client-side workload pump (~600 LOC mirroring
//!   `dpdk_burst.rs` + `dpdk_maxtp.rs` against the mTCP API) is
//!   tracked separately. See `peer/mtcp-driver.c` module docs for the
//!   frozen CLI + JSON contracts.
//!
//! Once the driver implementation lands, this Rust wrapper does NOT
//! change — the CLI / JSON schema is the seam.
//!
//! # Why not bind libmtcp.a in-process via FFI
//!
//! - libmtcp.a is built against DPDK 20.11. Linking it into a binary
//!   that also pulls DPDK 23.11 (via dpdk-net-sys) creates duplicate
//!   `rte_*` symbols with incompatible struct layouts → undefined
//!   behaviour at runtime.
//! - mTCP's `mtcp_init()` calls `rte_eal_init()`, which is a
//!   process-global one-time hook; you can't have two EAL instances.
//!
//! Subprocess is the only sound architecture.

use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::Value;

/// Errors returned by the mTCP driver wrapper. These map 1:1 to the
/// JSON `error` strings the C-side driver emits, plus harness-side
/// failure modes (driver missing, JSON parse error, …).
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// `--mtcp-driver-binary` does not exist or isn't executable on
    /// this host. Operator must run a rebake / `make -f Makefile.mtcp`
    /// before re-trying.
    #[error("mTCP driver binary `{path}` not found or not executable: {source}")]
    DriverMissing {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// Driver process spawned but exited non-zero, and stderr did not
    /// contain a parseable JSON error.
    #[error("mTCP driver `{path}` exited {code} (stderr: {stderr})")]
    DriverFailed {
        path: String,
        code: i32,
        stderr: String,
    },

    /// Driver returned ENOSYS — it's a stub, the workload pump is not
    /// yet implemented. Operator-facing message points at the
    /// follow-up task.
    #[error(
        "mtcp-driver returned ENOSYS — the client-side workload pump is a stub. \
         Build follow-up: tools/bench-vs-mtcp/peer/mtcp-driver.c. The Rust \
         subprocess wrapper, JSON schema, and AMI layout are landed and stable."
    )]
    DriverUnimplemented,

    /// Driver stdout is not parseable JSON, or the JSON is missing a
    /// required field.
    #[error("mTCP driver returned malformed JSON: {0}")]
    BadJson(String),

    /// Validation failure on the input config — caller passed empty /
    /// zero / non-absolute fields that the mtcp-driver wouldn't have
    /// accepted anyway.
    #[error("mtcp config invalid: {0}")]
    InvalidConfig(String),

    /// Generic I/O failure capturing the driver's stdout.
    #[error("I/O error talking to mtcp-driver: {0}")]
    Io(#[from] std::io::Error),
}

/// Configuration for an mTCP burst run. Mirrors `MaxtpConfig` for the
/// burst-side flags. `peer_binary` / `driver_binary` are kept separate
/// because the **server** runs `bench-peer` (on the peer host, via
/// SSH or pre-staged systemd unit) and the **client** invokes
/// `mtcp-driver` (on this host, in-process subprocess). Spec §11
/// orchestration leaves the peer-side launch to the bench-pair script.
#[derive(Debug, Clone)]
pub struct MtcpConfig<'a> {
    /// Peer host — IPv4 address for data-plane traffic (the peer host
    /// is already running `bench-peer` via the orchestrator).
    pub peer_ip: &'a str,
    pub peer_port: u16,
    /// Absolute path to the pre-installed server-side mTCP echo binary
    /// (`/opt/mtcp-peer/bench-peer` per spec §11). Validated only —
    /// this Rust process does not exec it; the peer host does.
    pub peer_binary: &'a str,
    /// Absolute path to the local mTCP client-side driver binary
    /// (`/opt/mtcp-peer/mtcp-driver` per spec §11). The Rust wrapper
    /// invokes this via `Command::new(driver_binary)`.
    pub driver_binary: &'a str,
    /// Absolute path to the mTCP startup config file (`-f` flag passed
    /// to `mtcp_init` inside the driver). Defaults to
    /// `/opt/mtcp/etc/mtcp.conf` on the AMI.
    pub mtcp_conf: &'a str,
    /// Burst size K in bytes.
    pub burst_bytes: u64,
    /// Idle gap G in milliseconds.
    pub gap_ms: u64,
    /// Burst count (post-warmup).
    pub bursts: u64,
    /// Warmup burst count (discarded).
    pub warmup: u64,
    /// MSS — must match dpdk_net side for the comparison to be valid.
    pub mss: u16,
    /// mTCP core count. Spec §11 picks 1 by default for the burst
    /// grid (single persistent connection).
    pub num_cores: u32,
    /// Hard timeout on the driver subprocess. If exceeded, the wrapper
    /// SIGKILLs the driver and surfaces `DriverFailed`.
    pub timeout: Duration,
}

/// Validate an [`MtcpConfig`] at the shape level. Returns `Err` with a
/// human-readable reason on any malformed field. Does NOT touch
/// libmtcp or the subprocess — safe to call in unit tests.
pub fn validate_config(cfg: &MtcpConfig<'_>) -> Result<(), String> {
    if cfg.peer_ip.is_empty() {
        return Err("mtcp: --peer-ip must be a non-empty address".to_string());
    }
    if cfg.peer_port == 0 {
        return Err("mtcp: --peer-port must be non-zero".to_string());
    }
    if cfg.peer_binary.is_empty() {
        return Err(
            "mtcp: --mtcp-peer-binary must be a non-empty path (expected \
             /opt/mtcp-peer/bench-peer on the baked AMI)"
                .to_string(),
        );
    }
    if !cfg.peer_binary.starts_with('/') {
        return Err(format!(
            "mtcp: --mtcp-peer-binary must be an absolute path, got `{}`",
            cfg.peer_binary
        ));
    }
    if cfg.driver_binary.is_empty() {
        return Err(
            "mtcp: --mtcp-driver-binary must be a non-empty path (expected \
             /opt/mtcp-peer/mtcp-driver on the baked AMI)"
                .to_string(),
        );
    }
    if !cfg.driver_binary.starts_with('/') {
        return Err(format!(
            "mtcp: --mtcp-driver-binary must be an absolute path, got `{}`",
            cfg.driver_binary
        ));
    }
    if cfg.mtcp_conf.is_empty() {
        return Err("mtcp: --mtcp-conf must be a non-empty path".to_string());
    }
    if !cfg.mtcp_conf.starts_with('/') {
        return Err(format!(
            "mtcp: --mtcp-conf must be an absolute path, got `{}`",
            cfg.mtcp_conf
        ));
    }
    if cfg.burst_bytes == 0 {
        return Err("mtcp: burst_bytes (K) must be non-zero".to_string());
    }
    if cfg.bursts == 0 {
        return Err("mtcp: bursts must be non-zero".to_string());
    }
    if cfg.mss == 0 {
        return Err("mtcp: mss must be non-zero".to_string());
    }
    if cfg.num_cores == 0 {
        return Err("mtcp: num_cores must be non-zero".to_string());
    }
    if cfg.timeout.is_zero() {
        return Err("mtcp: timeout must be non-zero".to_string());
    }
    Ok(())
}

/// Build the argv for a burst-grid invocation. Pure function so unit
/// tests can verify the exact CLI shape against the C driver's frozen
/// long-options table without a subprocess spawn.
pub fn build_burst_argv(cfg: &MtcpConfig<'_>) -> Vec<String> {
    vec![
        "--workload".into(),
        "burst".into(),
        "--mtcp-conf".into(),
        cfg.mtcp_conf.into(),
        "--peer-ip".into(),
        cfg.peer_ip.into(),
        "--peer-port".into(),
        cfg.peer_port.to_string(),
        "--mss".into(),
        cfg.mss.to_string(),
        "--num-cores".into(),
        cfg.num_cores.to_string(),
        "--burst-bytes".into(),
        cfg.burst_bytes.to_string(),
        "--gap-ms".into(),
        cfg.gap_ms.to_string(),
        "--bursts".into(),
        cfg.bursts.to_string(),
        "--warmup".into(),
        cfg.warmup.to_string(),
    ]
}

/// Burst-workload entry point. Spawns the mtcp-driver subprocess with
/// the burst-grid argv, captures stdout, parses the JSON. Returns the
/// per-burst sample list (bps).
///
/// Today the driver is a STUB that returns ENOSYS — this function
/// surfaces that as `Error::DriverUnimplemented` so the caller
/// degrades gracefully (lenient mode WARN + skip; strict mode hard
/// error). All the wrapper logic — validation, argv construction,
/// stderr parsing, timeout — is exercised by the stub today, so when
/// the real driver lands no Rust-side work is required.
pub fn run_burst_workload(cfg: &MtcpConfig<'_>) -> Result<Vec<f64>, Error> {
    if let Err(reason) = validate_config(cfg) {
        return Err(Error::InvalidConfig(reason));
    }
    let argv = build_burst_argv(cfg);
    let json = invoke_driver(cfg.driver_binary, &argv, cfg.timeout)?;
    parse_burst_json(&json)
}

/// Configuration for an mTCP maxtp (W × C) run. Mirrors
/// [`MtcpConfig`] for the burst workload.
#[derive(Debug, Clone)]
pub struct MaxtpConfig<'a> {
    pub peer_ip: &'a str,
    pub peer_port: u16,
    pub peer_binary: &'a str,
    pub driver_binary: &'a str,
    pub mtcp_conf: &'a str,
    pub write_bytes: u64,
    pub conn_count: u64,
    pub warmup_secs: u64,
    pub duration_secs: u64,
    pub mss: u16,
    pub num_cores: u32,
    pub timeout: Duration,
}

/// Validate a [`MaxtpConfig`] at the shape level.
pub fn validate_maxtp_config(cfg: &MaxtpConfig<'_>) -> Result<(), String> {
    if cfg.peer_ip.is_empty() {
        return Err("mtcp: --peer-ip must be a non-empty address".to_string());
    }
    if cfg.peer_port == 0 {
        return Err("mtcp: --peer-port must be non-zero".to_string());
    }
    if cfg.peer_binary.is_empty() {
        return Err(
            "mtcp: --mtcp-peer-binary must be a non-empty path (expected \
             /opt/mtcp-peer/bench-peer on the baked AMI)"
                .to_string(),
        );
    }
    if !cfg.peer_binary.starts_with('/') {
        return Err(format!(
            "mtcp: --mtcp-peer-binary must be an absolute path, got `{}`",
            cfg.peer_binary
        ));
    }
    if cfg.driver_binary.is_empty() {
        return Err("mtcp: --mtcp-driver-binary must be a non-empty path".to_string());
    }
    if !cfg.driver_binary.starts_with('/') {
        return Err(format!(
            "mtcp: --mtcp-driver-binary must be an absolute path, got `{}`",
            cfg.driver_binary
        ));
    }
    if cfg.mtcp_conf.is_empty() {
        return Err("mtcp: --mtcp-conf must be a non-empty path".to_string());
    }
    if !cfg.mtcp_conf.starts_with('/') {
        return Err(format!(
            "mtcp: --mtcp-conf must be an absolute path, got `{}`",
            cfg.mtcp_conf
        ));
    }
    if cfg.write_bytes == 0 {
        return Err("mtcp: write_bytes (W) must be non-zero".to_string());
    }
    if cfg.conn_count == 0 {
        return Err("mtcp: conn_count (C) must be non-zero".to_string());
    }
    if cfg.duration_secs == 0 {
        return Err("mtcp: duration_secs (T) must be non-zero".to_string());
    }
    if cfg.mss == 0 {
        return Err("mtcp: mss must be non-zero".to_string());
    }
    if cfg.num_cores == 0 {
        return Err("mtcp: num_cores must be non-zero".to_string());
    }
    if cfg.timeout.is_zero() {
        return Err("mtcp: timeout must be non-zero".to_string());
    }
    Ok(())
}

/// Build the argv for a maxtp-grid invocation.
pub fn build_maxtp_argv(cfg: &MaxtpConfig<'_>) -> Vec<String> {
    vec![
        "--workload".into(),
        "maxtp".into(),
        "--mtcp-conf".into(),
        cfg.mtcp_conf.into(),
        "--peer-ip".into(),
        cfg.peer_ip.into(),
        "--peer-port".into(),
        cfg.peer_port.to_string(),
        "--mss".into(),
        cfg.mss.to_string(),
        "--num-cores".into(),
        cfg.num_cores.to_string(),
        "--write-bytes".into(),
        cfg.write_bytes.to_string(),
        "--conn-count".into(),
        cfg.conn_count.to_string(),
        "--warmup-secs".into(),
        cfg.warmup_secs.to_string(),
        "--duration-secs".into(),
        cfg.duration_secs.to_string(),
    ]
}

/// Maxtp-workload entry point. Returns `(goodput_bps, pps)`.
pub fn run_maxtp_workload(cfg: &MaxtpConfig<'_>) -> Result<(f64, f64), Error> {
    if let Err(reason) = validate_maxtp_config(cfg) {
        return Err(Error::InvalidConfig(reason));
    }
    let argv = build_maxtp_argv(cfg);
    let json = invoke_driver(cfg.driver_binary, &argv, cfg.timeout)?;
    parse_maxtp_json(&json)
}

/// Spawn the driver subprocess, wait up to `timeout`, capture stdout +
/// stderr. Return stdout on success or one of the structured Error
/// variants on failure. Stderr-as-JSON ENOSYS maps to
/// `DriverUnimplemented`.
fn invoke_driver(binary: &str, argv: &[String], timeout: Duration) -> Result<String, Error> {
    let mut cmd = Command::new(binary);
    cmd.args(argv);
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let child = cmd.spawn().map_err(|e| Error::DriverMissing {
        path: binary.to_string(),
        source: e,
    })?;

    // No native timeout in std — we wait synchronously. If a real
    // operator-facing timeout is needed, we can graft `wait_timeout`
    // crate; today the driver is a stub returning ENOSYS instantly so
    // synchronous wait is fine. The timeout field is preserved on the
    // config so the contract doesn't break when we add the crate.
    let _ = timeout; // accepted, unused in the stub-only path

    let output = child.wait_with_output()?;

    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }

    // Non-zero exit. Try to parse stderr as JSON to recover the
    // structured error code.
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
    let code = output.status.code().unwrap_or(-1);

    if let Ok(v) = serde_json::from_str::<Value>(&stderr) {
        if let Some(errno) = v.get("errno").and_then(|x| x.as_i64()) {
            if errno == 38
            /* ENOSYS */
            {
                return Err(Error::DriverUnimplemented);
            }
        }
    }

    Err(Error::DriverFailed {
        path: binary.to_string(),
        code,
        stderr,
    })
}

/// Parse the burst-workload JSON into a per-burst bps list.
fn parse_burst_json(s: &str) -> Result<Vec<f64>, Error> {
    let v: Value = serde_json::from_str(s).map_err(|e| Error::BadJson(format!("not JSON: {e}")))?;
    let workload = v
        .get("workload")
        .and_then(|x| x.as_str())
        .ok_or_else(|| Error::BadJson("missing `workload`".into()))?;
    if workload != "burst" {
        return Err(Error::BadJson(format!(
            "expected workload=burst, got `{workload}`"
        )));
    }
    let arr = v
        .get("samples_bps")
        .and_then(|x| x.as_array())
        .ok_or_else(|| Error::BadJson("missing `samples_bps`".into()))?;
    let mut out = Vec::with_capacity(arr.len());
    for x in arr {
        let f = x
            .as_f64()
            .ok_or_else(|| Error::BadJson("samples_bps element not f64".into()))?;
        if !f.is_finite() || f < 0.0 {
            return Err(Error::BadJson(format!(
                "samples_bps element not a non-negative finite f64: {f}"
            )));
        }
        out.push(f);
    }
    Ok(out)
}

/// Parse the maxtp-workload JSON into `(goodput_bps, pps)`.
fn parse_maxtp_json(s: &str) -> Result<(f64, f64), Error> {
    let v: Value = serde_json::from_str(s).map_err(|e| Error::BadJson(format!("not JSON: {e}")))?;
    let workload = v
        .get("workload")
        .and_then(|x| x.as_str())
        .ok_or_else(|| Error::BadJson("missing `workload`".into()))?;
    if workload != "maxtp" {
        return Err(Error::BadJson(format!(
            "expected workload=maxtp, got `{workload}`"
        )));
    }
    let bps = v
        .get("goodput_bps")
        .and_then(|x| x.as_f64())
        .ok_or_else(|| Error::BadJson("missing `goodput_bps`".into()))?;
    let pps = v
        .get("pps")
        .and_then(|x| x.as_f64())
        .ok_or_else(|| Error::BadJson("missing `pps`".into()))?;
    if !bps.is_finite() || bps < 0.0 {
        return Err(Error::BadJson(format!(
            "goodput_bps not finite/non-neg: {bps}"
        )));
    }
    if !pps.is_finite() || pps < 0.0 {
        return Err(Error::BadJson(format!("pps not finite/non-neg: {pps}")));
    }
    Ok((bps, pps))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn good_cfg() -> MtcpConfig<'static> {
        MtcpConfig {
            peer_ip: "10.0.0.42",
            peer_port: 10_001,
            peer_binary: "/opt/mtcp-peer/bench-peer",
            driver_binary: "/opt/mtcp-peer/mtcp-driver",
            mtcp_conf: "/opt/mtcp/etc/mtcp.conf",
            burst_bytes: 64 * 1024,
            gap_ms: 0,
            bursts: 10_000,
            warmup: 100,
            mss: 1460,
            num_cores: 1,
            timeout: Duration::from_secs(60),
        }
    }

    fn good_maxtp_cfg() -> MaxtpConfig<'static> {
        MaxtpConfig {
            peer_ip: "10.0.0.42",
            peer_port: 10_001,
            peer_binary: "/opt/mtcp-peer/bench-peer",
            driver_binary: "/opt/mtcp-peer/mtcp-driver",
            mtcp_conf: "/opt/mtcp/etc/mtcp.conf",
            write_bytes: 4096,
            conn_count: 4,
            warmup_secs: 10,
            duration_secs: 60,
            mss: 1460,
            num_cores: 1,
            timeout: Duration::from_secs(120),
        }
    }

    // ---- validate_config ------------------------------------------

    #[test]
    fn validate_config_accepts_good_cfg() {
        assert!(validate_config(&good_cfg()).is_ok());
    }

    #[test]
    fn validate_config_rejects_empty_peer_ip() {
        let mut c = good_cfg();
        c.peer_ip = "";
        let err = validate_config(&c).unwrap_err();
        assert!(err.contains("peer-ip"));
    }

    #[test]
    fn validate_config_rejects_zero_port() {
        let mut c = good_cfg();
        c.peer_port = 0;
        assert!(validate_config(&c).is_err());
    }

    #[test]
    fn validate_config_rejects_empty_peer_binary() {
        let mut c = good_cfg();
        c.peer_binary = "";
        let err = validate_config(&c).unwrap_err();
        assert!(err.contains("peer-binary"));
    }

    #[test]
    fn validate_config_rejects_relative_peer_binary() {
        let mut c = good_cfg();
        c.peer_binary = "bench-peer";
        let err = validate_config(&c).unwrap_err();
        assert!(err.contains("absolute path"));
    }

    #[test]
    fn validate_config_rejects_relative_driver_binary() {
        let mut c = good_cfg();
        c.driver_binary = "mtcp-driver";
        let err = validate_config(&c).unwrap_err();
        assert!(err.contains("absolute path"));
    }

    #[test]
    fn validate_config_rejects_empty_driver_binary() {
        let mut c = good_cfg();
        c.driver_binary = "";
        let err = validate_config(&c).unwrap_err();
        assert!(err.contains("driver-binary"));
    }

    #[test]
    fn validate_config_rejects_relative_mtcp_conf() {
        let mut c = good_cfg();
        c.mtcp_conf = "mtcp.conf";
        let err = validate_config(&c).unwrap_err();
        assert!(err.contains("absolute path"));
    }

    #[test]
    fn validate_config_rejects_zero_burst_bytes() {
        let mut c = good_cfg();
        c.burst_bytes = 0;
        assert!(validate_config(&c).is_err());
    }

    #[test]
    fn validate_config_rejects_zero_bursts() {
        let mut c = good_cfg();
        c.bursts = 0;
        assert!(validate_config(&c).is_err());
    }

    #[test]
    fn validate_config_rejects_zero_mss() {
        let mut c = good_cfg();
        c.mss = 0;
        assert!(validate_config(&c).is_err());
    }

    #[test]
    fn validate_config_rejects_zero_num_cores() {
        let mut c = good_cfg();
        c.num_cores = 0;
        assert!(validate_config(&c).is_err());
    }

    #[test]
    fn validate_config_rejects_zero_timeout() {
        let mut c = good_cfg();
        c.timeout = Duration::ZERO;
        assert!(validate_config(&c).is_err());
    }

    // ---- validate_maxtp_config ------------------------------------

    #[test]
    fn validate_maxtp_config_accepts_good_cfg() {
        assert!(validate_maxtp_config(&good_maxtp_cfg()).is_ok());
    }

    #[test]
    fn validate_maxtp_config_rejects_empty_peer_ip() {
        let mut c = good_maxtp_cfg();
        c.peer_ip = "";
        assert!(validate_maxtp_config(&c).is_err());
    }

    #[test]
    fn validate_maxtp_config_rejects_zero_port() {
        let mut c = good_maxtp_cfg();
        c.peer_port = 0;
        assert!(validate_maxtp_config(&c).is_err());
    }

    #[test]
    fn validate_maxtp_config_rejects_relative_peer_binary() {
        let mut c = good_maxtp_cfg();
        c.peer_binary = "bench-peer";
        let err = validate_maxtp_config(&c).unwrap_err();
        assert!(err.contains("absolute path"));
    }

    #[test]
    fn validate_maxtp_config_rejects_zero_write_bytes() {
        let mut c = good_maxtp_cfg();
        c.write_bytes = 0;
        assert!(validate_maxtp_config(&c).is_err());
    }

    #[test]
    fn validate_maxtp_config_rejects_zero_conn_count() {
        let mut c = good_maxtp_cfg();
        c.conn_count = 0;
        assert!(validate_maxtp_config(&c).is_err());
    }

    #[test]
    fn validate_maxtp_config_rejects_zero_duration() {
        let mut c = good_maxtp_cfg();
        c.duration_secs = 0;
        assert!(validate_maxtp_config(&c).is_err());
    }

    #[test]
    fn validate_maxtp_config_rejects_zero_mss() {
        let mut c = good_maxtp_cfg();
        c.mss = 0;
        assert!(validate_maxtp_config(&c).is_err());
    }

    // ---- argv construction ----------------------------------------

    #[test]
    fn burst_argv_contains_required_flags() {
        let argv = build_burst_argv(&good_cfg());
        let joined = argv.join(" ");
        assert!(joined.contains("--workload burst"));
        assert!(joined.contains("--mtcp-conf /opt/mtcp/etc/mtcp.conf"));
        assert!(joined.contains("--peer-ip 10.0.0.42"));
        assert!(joined.contains("--peer-port 10001"));
        assert!(joined.contains("--mss 1460"));
        assert!(joined.contains("--num-cores 1"));
        assert!(joined.contains("--burst-bytes 65536"));
        assert!(joined.contains("--gap-ms 0"));
        assert!(joined.contains("--bursts 10000"));
        assert!(joined.contains("--warmup 100"));
    }

    #[test]
    fn maxtp_argv_contains_required_flags() {
        let argv = build_maxtp_argv(&good_maxtp_cfg());
        let joined = argv.join(" ");
        assert!(joined.contains("--workload maxtp"));
        assert!(joined.contains("--peer-ip 10.0.0.42"));
        assert!(joined.contains("--peer-port 10001"));
        assert!(joined.contains("--write-bytes 4096"));
        assert!(joined.contains("--conn-count 4"));
        assert!(joined.contains("--warmup-secs 10"));
        assert!(joined.contains("--duration-secs 60"));
    }

    // ---- run paths surface invalid config / missing driver --------

    #[test]
    fn run_burst_workload_rejects_invalid_cfg() {
        let mut c = good_cfg();
        c.peer_ip = "";
        let err = run_burst_workload(&c).unwrap_err();
        assert!(matches!(err, Error::InvalidConfig(_)));
    }

    #[test]
    fn run_maxtp_workload_rejects_invalid_cfg() {
        let mut c = good_maxtp_cfg();
        c.peer_ip = "";
        let err = run_maxtp_workload(&c).unwrap_err();
        assert!(matches!(err, Error::InvalidConfig(_)));
    }

    #[test]
    fn run_burst_workload_surfaces_missing_driver() {
        let mut c = good_cfg();
        c.driver_binary = "/this/path/does/not/exist/mtcp-driver";
        let err = run_burst_workload(&c).unwrap_err();
        assert!(matches!(err, Error::DriverMissing { .. }));
    }

    #[test]
    fn run_maxtp_workload_surfaces_missing_driver() {
        let mut c = good_maxtp_cfg();
        c.driver_binary = "/this/path/does/not/exist/mtcp-driver";
        let err = run_maxtp_workload(&c).unwrap_err();
        assert!(matches!(err, Error::DriverMissing { .. }));
    }

    // ---- JSON parsers -------------------------------------------

    #[test]
    fn parse_burst_json_round_trip() {
        let s = r#"{"workload":"burst","samples_bps":[1.0e9, 2.5e9, 4.2e9],"tx_ts_mode":"tsc_fallback","bytes_sent_total":3,"bytes_acked_total":3}"#;
        let v = parse_burst_json(s).unwrap();
        assert_eq!(v.len(), 3);
        assert_eq!(v[0], 1.0e9);
        assert_eq!(v[2], 4.2e9);
    }

    #[test]
    fn parse_burst_json_rejects_wrong_workload() {
        let s = r#"{"workload":"maxtp","samples_bps":[1.0]}"#;
        assert!(matches!(parse_burst_json(s), Err(Error::BadJson(_))));
    }

    #[test]
    fn parse_burst_json_rejects_negative_sample() {
        let s = r#"{"workload":"burst","samples_bps":[-1.0]}"#;
        assert!(matches!(parse_burst_json(s), Err(Error::BadJson(_))));
    }

    #[test]
    fn parse_burst_json_rejects_nan_sample() {
        let s = r#"{"workload":"burst","samples_bps":[null]}"#;
        assert!(matches!(parse_burst_json(s), Err(Error::BadJson(_))));
    }

    #[test]
    fn parse_burst_json_rejects_missing_field() {
        let s = r#"{"workload":"burst"}"#;
        assert!(matches!(parse_burst_json(s), Err(Error::BadJson(_))));
    }

    #[test]
    fn parse_burst_json_rejects_garbage() {
        assert!(matches!(
            parse_burst_json("not json"),
            Err(Error::BadJson(_))
        ));
    }

    #[test]
    fn parse_maxtp_json_round_trip() {
        let s = r#"{"workload":"maxtp","goodput_bps":2.4e10,"pps":1.6e6,"tx_ts_mode":"n/a","bytes_sent_total":1234}"#;
        let (bps, pps) = parse_maxtp_json(s).unwrap();
        assert_eq!(bps, 2.4e10);
        assert_eq!(pps, 1.6e6);
    }

    #[test]
    fn parse_maxtp_json_rejects_wrong_workload() {
        let s = r#"{"workload":"burst","goodput_bps":1.0,"pps":1.0}"#;
        assert!(matches!(parse_maxtp_json(s), Err(Error::BadJson(_))));
    }

    #[test]
    fn parse_maxtp_json_rejects_missing_pps() {
        let s = r#"{"workload":"maxtp","goodput_bps":1.0}"#;
        assert!(matches!(parse_maxtp_json(s), Err(Error::BadJson(_))));
    }
}
