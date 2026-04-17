//! Pure-Rust internals of the resd.dpdk_tcp stack.
//! The public `extern "C"` surface lives in the `resd-net` crate.

pub mod clock;
pub mod counters;
pub mod engine;
pub mod error;
pub mod mempool;

pub use error::Error;
