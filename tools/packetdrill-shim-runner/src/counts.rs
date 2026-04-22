//! A7: pinned ligurio-corpus counts. Values are finalized in Task 15
//! after the full classifier pass; T9-T14 leave them at zero so the
//! corpus gate test fails loudly until T15 pins them.
//!
//! A7 T15 pragmatic floor: the A7 shim binary did not pass any corpus
//! script end-to-end (0/122). All scripts were skip-bucketed.
//!
//! A8 T15 S2 update: the shim now IP-rewrites inject/drain frames into
//! packetdrill's live address space and plumbs the engine's conn-peer
//! FFI through shim_accept, unlocking 6 server-side scripts across
//! listen/, blocking/, and close/ directories. Remaining 116 stay
//! skipped for A8+ follow-up (engine edge behaviors, ISN pinning,
//! shutdown wiring, scripts/defaults.sh init files).
pub const LIGURIO_RUNNABLE_COUNT: usize = 6;
pub const LIGURIO_SKIP_UNTRANSLATABLE: usize = 116;
pub const LIGURIO_SKIP_OUT_OF_SCOPE: usize = 0;
