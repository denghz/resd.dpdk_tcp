//! bench-rx-burst build script — links libfstack.a when the `fstack`
//! cargo feature is enabled. No-op otherwise so default workspace
//! builds don't require F-Stack on the dev host.
//!
//! Phase 8 of the 2026-05-09 bench-suite overhaul. Recipe is verbatim
//! from bench-tx-burst/build.rs; the F-Stack link layout requires a
//! specific argument ORDER (whole-archive libfstack.a sandwiched between
//! `-Wl,--push-state,--no-as-needed` / `-Wl,--pop-state`) that Cargo's
//! link-lib emit can't express.
//!
//! DPDK is intentionally NOT re-linked here. dpdk-net-sys (a transitive
//! dep via dpdk-net-core) already links DPDK via pkg-config. Adding a
//! second DPDK whole-archive block duplicates the tailq constructor
//! objects and triggers a fatal `RTE_MBUF_DYNFIELD tailq is already
//! registered` PANIC at binary startup.

fn main() {
    println!("cargo:rerun-if-env-changed=FF_PATH");
    println!("cargo:rerun-if-changed=build.rs");

    if std::env::var_os("CARGO_FEATURE_FSTACK").is_none() {
        // Default build path — no F-Stack linkage.
        return;
    }

    let ff_path = std::env::var("FF_PATH").unwrap_or_else(|_| "/opt/f-stack".to_string());
    println!("cargo:rustc-link-search=native={ff_path}/lib");
    println!("cargo:rustc-link-arg=-Wl,-z,nostart-stop-gc");
    println!("cargo:rustc-link-arg=-Wl,--whole-archive");
    println!("cargo:rustc-link-arg=-lfstack");
    println!("cargo:rustc-link-arg=-Wl,--no-whole-archive");

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
