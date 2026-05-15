//! Build-time helpers for bench-* binaries that link libfstack.a.
//!
//! Used as a `[build-dependencies]` entry on each consumer crate's
//! `Cargo.toml`. Each consumer's `build.rs` calls
//! `emit_fstack_link_args_if_enabled()` which conditionally emits the
//! linker pragmas required to link `libfstack.a` with the right
//! whole-archive sandwich shape — and to re-link DPDK's `rte_*`
//! libraries under `--no-as-needed` so they satisfy libfstack.a's
//! undefined references at runtime.
//!
//! This crate exists because Cargo's `cargo:rustc-link-lib=...` emit
//! cannot express the argument ORDER required (push-state /
//! --no-as-needed / --whole-archive / -lfstack / --no-whole-archive /
//! DPDK rte_* libs under push-state / --pop-state). The pragmas must
//! be emitted as `rustc-link-arg=...` in sequence, which is mechanical
//! but error-prone if duplicated across N consumers.
//!
//! See `tools/bench-fstack-ffi/src/lib.rs` for the runtime FFI
//! bindings; this crate is build-time only.
//!
//! # DPDK is intentionally NOT re-whole-archived here
//!
//! `dpdk-net-sys` (a transitive dep via `dpdk-net-core`) already links
//! DPDK via pkg-config before this build.rs runs. With `--as-needed`
//! those libs are excluded from `DT_NEEDED` when processed; the
//! `--no-as-needed` block below re-links them so they satisfy
//! libfstack.a's undefined references. Adding a second DPDK
//! whole-archive block would duplicate the tailq constructor objects
//! and trigger a fatal `RTE_MBUF_DYNFIELD tailq is already registered`
//! PANIC at binary startup.
//!
//! # F-Stack install layout
//!
//! image-builder component `04b-install-f-stack.yaml` installs at:
//!   /opt/f-stack/lib/libfstack.a
//!   /opt/f-stack/include/  (ff_api.h, ff_config.h, ...)
//!
//! Operators with F-Stack installed elsewhere can override via the
//! `FF_PATH` env var (matches F-Stack's upstream Makefile convention).

/// Emit the libfstack.a + DPDK link-arg sequence iff the `fstack`
/// cargo feature is enabled on the calling crate.
///
/// Reads `CARGO_FEATURE_FSTACK` (set by Cargo when the calling crate
/// is built with `--features fstack`). The helper does **not** emit
/// `cargo:rerun-if-env-changed=CARGO_FEATURE_FSTACK` — Cargo handles
/// feature-flag re-runs automatically. Callers should still emit
/// their own `cargo:rerun-if-env-changed=FF_PATH` and
/// `cargo:rerun-if-changed=build.rs` so this helper's emit set stays
/// focused on the link-arg sequence.
pub fn emit_fstack_link_args_if_enabled() {
    if std::env::var_os("CARGO_FEATURE_FSTACK").is_none() {
        // Default build path — no F-Stack linkage.
        return;
    }
    emit_fstack_link_args();
}

/// Emit the libfstack.a + DPDK link-arg sequence unconditionally.
/// Prefer `emit_fstack_link_args_if_enabled` unless the caller has
/// already gated on the `fstack` feature.
///
/// The emit ORDER is load-bearing — see the module-level docs.
pub fn emit_fstack_link_args() {
    let ff_path = std::env::var("FF_PATH").unwrap_or_else(|_| "/opt/f-stack".to_string());
    println!("cargo:rustc-link-search=native={ff_path}/lib");
    println!("cargo:rustc-link-arg=-Wl,-z,nostart-stop-gc");
    println!("cargo:rustc-link-arg=-Wl,--whole-archive");
    println!("cargo:rustc-link-arg=-lfstack");
    println!("cargo:rustc-link-arg=-Wl,--no-whole-archive");

    // dpdk-net-sys links DPDK via pkg-config before this build.rs runs.
    // With --as-needed those libs are excluded from DT_NEEDED when
    // processed; re-link them with --no-as-needed so they satisfy
    // libfstack.a's undefined references at runtime.
    println!("cargo:rustc-link-arg=-Wl,--push-state,--no-as-needed");
    for lib in [
        "rte_ring", "rte_mempool", "rte_mbuf", "rte_eal",
        "rte_ethdev", "rte_net", "rte_pci", "rte_timer",
        "rte_kvargs", "rte_telemetry", "rte_log",
        "rte_net_bond",
        "crypto",
    ] {
        println!("cargo:rustc-link-arg=-l{lib}");
    }
    println!("cargo:rustc-link-arg=-Wl,--pop-state");

    for lib in ["rt", "m", "dl", "crypto", "pthread", "numa"] {
        println!("cargo:rustc-link-lib={lib}");
    }
    // libc tail-link, see legacy bench-vs-mtcp/build.rs for rationale.
    println!("cargo:rustc-link-arg=-lc");
}
