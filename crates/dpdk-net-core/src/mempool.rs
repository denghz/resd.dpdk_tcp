use dpdk_net_sys as sys;
use std::ffi::CString;
use std::ptr::NonNull;

use crate::Error;

/// RAII wrapper around an `rte_mempool*`.
/// Dropped pool calls `rte_mempool_free` on the inner pointer.
pub struct Mempool {
    ptr: NonNull<sys::rte_mempool>,
    name: CString,
}

impl Mempool {
    /// Create a packet-mbuf pool with DPDK defaults + configurable headroom.
    pub fn new_pktmbuf(
        name: &str,
        n_elements: u32,
        cache_size: u32,
        priv_size: u16,
        data_room_size: u16,
        socket_id: i32,
    ) -> Result<Self, Error> {
        let cname = CString::new(name).map_err(|_| Error::MempoolCreate("name contains NUL"))?;
        // Safety: passing valid parameters to an FFI function; DPDK must be initialized
        // (EAL) before this. Caller responsibility.
        let p = unsafe {
            sys::rte_pktmbuf_pool_create(
                cname.as_ptr(),
                n_elements,
                cache_size,
                priv_size,
                data_room_size,
                socket_id,
            )
        };
        let ptr = NonNull::new(p).ok_or(Error::MempoolCreate(
            "rte_pktmbuf_pool_create returned NULL",
        ))?;
        Ok(Self { ptr, name: cname })
    }

    #[inline]
    pub fn as_ptr(&self) -> *mut sys::rte_mempool {
        self.ptr.as_ptr()
    }

    pub fn name(&self) -> &std::ffi::CStr {
        &self.name
    }
}

impl Drop for Mempool {
    fn drop(&mut self) {
        // Safety: we own the mempool; no other references should exist because
        // we hold NonNull and never handed it out beyond &mut self.
        unsafe { sys::rte_mempool_free(self.ptr.as_ptr()) };
    }
}

// Pools are created on one lcore but passed between lcores at setup time;
// mempool operations themselves are thread-safe per DPDK docs.
unsafe impl Send for Mempool {}
unsafe impl Sync for Mempool {}

/// Non-owning handle to an rte_mbuf. Does NOT manage refcount — the engine
/// explicitly alloc/frees and increments refcnt when stashing into
/// `SendRetrans`. Per spec §7.2 design: keep unsafe pointer work in
/// `engine.rs` so this module stays safe-code-only.
///
/// `Clone` is cheap (pointer copy) but does NOT refcnt_inc; the caller must
/// call `rte_mbuf_refcnt_update` explicitly where refcount ownership is
/// transferred. Tests use `null_for_test()` — the null pointer is never
/// dereferenced in unit tests (SendRetrans just stores it).
#[derive(Clone, Copy)]
pub struct Mbuf {
    ptr: *mut sys::rte_mbuf,
}

impl Mbuf {
    /// Wrap a raw pointer. Caller owns the refcount responsibility.
    #[inline]
    pub fn from_ptr(ptr: *mut sys::rte_mbuf) -> Self {
        Self { ptr }
    }

    #[inline]
    pub fn as_ptr(&self) -> *mut sys::rte_mbuf {
        self.ptr
    }

    /// Test-only null handle. The pointer is never dereferenced in tests.
    #[cfg(test)]
    pub fn null_for_test() -> Self {
        Self {
            ptr: std::ptr::null_mut(),
        }
    }
}

// Safety: mbuf pointers are moved between threads only in ways the engine
// serializes; `SendRetrans` lives per-conn inside `TcpConn`, which is
// accessed by one engine lcore at a time. Matches `Mempool`'s Send impl.
unsafe impl Send for Mbuf {}
