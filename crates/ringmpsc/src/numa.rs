//! NUMA-aware ring buffer memory allocation.
//!
//! The [`NumaAllocator`] binds ring buffer backing memory to specific NUMA
//! nodes on Linux via `libc::mbind`. On non-Linux platforms, it falls back
//! to the default [`HeapAllocator`](super::HeapAllocator).
//!
//! # Policies
//!
//! [`NumaPolicy`] controls how NUMA nodes are assigned per `allocate()` call:
//!
//! - [`Fixed(node)`](NumaPolicy::Fixed) — all allocations on one node
//! - [`RoundRobin`](NumaPolicy::RoundRobin) — cycle across available nodes
//! - [`ProducerLocal`](NumaPolicy::ProducerLocal) — allocate on the calling
//!   thread's local node
//!
//! # Example
//!
//! ```ignore
//! use ringmpsc_rs::{Channel, Config, numa::{NumaAllocator, NumaPolicy}};
//!
//! // Distribute ring buffers across NUMA nodes
//! let channel = Channel::<u64, NumaAllocator>::new_in(
//!     Config::default(),
//!     NumaAllocator::new(NumaPolicy::RoundRobin),
//! );
//! ```

use crate::allocator::BufferAllocator;
use std::mem::MaybeUninit;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;

/// NUMA memory placement policy for ring buffer allocation.
#[derive(Debug, Clone)]
pub enum NumaPolicy {
    /// All allocations placed on a single, specified NUMA node.
    Fixed(u16),

    /// Distribute allocations across NUMA nodes in round-robin order.
    /// Each `allocate()` call advances to the next node.
    RoundRobin,

    /// Allocate on the NUMA node local to the calling thread's CPU.
    ///
    /// **Caveat with `Channel`**: `Channel::new_in()` pre-allocates all rings
    /// at construction time on a single thread, so `ProducerLocal` resolves to
    /// the *channel creator's* node. For true per-producer placement, construct
    /// individual `Ring::new_in()` instances from each producer thread.
    ProducerLocal,
}

/// Cached NUMA topology information.
#[derive(Debug, Clone)]
struct NumaInfo {
    /// Number of available NUMA nodes.
    num_nodes: u16,
}

impl NumaInfo {
    fn detect() -> Self {
        NumaInfo {
            num_nodes: detect_num_nodes(),
        }
    }
}

/// NUMA-aware ring buffer allocator.
///
/// Implements [`BufferAllocator`] by allocating memory via `mmap` and binding
/// it to a specific NUMA node via `mbind` on Linux. On non-Linux platforms,
/// falls back to heap allocation (INV-NUMA-02).
///
/// # Safety
///
/// This allocator satisfies the [`BufferAllocator`] safety contract (INV-MEM-04):
/// 1. `allocate(capacity)` returns exactly `capacity` elements.
/// 2. Memory is valid for reads/writes for the buffer's lifetime.
/// 3. `Deref`/`DerefMut` target contiguous slices.
/// 4. `Drop` correctly deallocates via `munmap` (Linux) or `Box` (fallback).
#[derive(Debug, Clone)]
pub struct NumaAllocator {
    #[allow(dead_code)] // used in platform::BufferAllocator impl
    policy: NumaPolicy,
    info: NumaInfo,
    /// Shared counter for RoundRobin policy. Clones share the same counter
    /// so sequential `allocate()` calls distribute across nodes.
    #[allow(dead_code)] // used in platform::BufferAllocator impl
    rr_counter: Arc<AtomicU16>,
    /// Whether to use huge pages (MAP_HUGETLB) for TLB optimization.
    pub huge_pages: bool,
}

impl NumaAllocator {
    /// Create a new NUMA allocator with the given placement policy.
    #[must_use]
    pub fn new(policy: NumaPolicy) -> Self {
        Self {
            policy,
            info: NumaInfo::detect(),
            rr_counter: Arc::new(AtomicU16::new(0)),
            huge_pages: false,
        }
    }

    /// Create a new NUMA allocator with huge page support enabled.
    ///
    /// Adds `MAP_HUGETLB` to `mmap` flags on Linux for 2 MiB pages,
    /// reducing TLB misses on large ring buffers.
    #[must_use]
    pub fn with_huge_pages(policy: NumaPolicy) -> Self {
        Self {
            policy,
            info: NumaInfo::detect(),
            rr_counter: Arc::new(AtomicU16::new(0)),
            huge_pages: true,
        }
    }

    /// Returns the number of detected NUMA nodes.
    #[must_use]
    pub fn num_nodes(&self) -> u16 {
        self.info.num_nodes
    }

    /// Returns true if the system has multiple NUMA nodes.
    #[must_use]
    pub fn is_numa_available(&self) -> bool {
        self.info.num_nodes > 1
    }

    /// Resolve the target NUMA node for the next allocation.
    #[allow(dead_code)] // used in platform::BufferAllocator impl
    fn resolve_node(&self) -> u16 {
        match &self.policy {
            NumaPolicy::Fixed(node) => *node,
            NumaPolicy::RoundRobin => {
                let idx = self.rr_counter.fetch_add(1, Ordering::Relaxed);
                idx % self.info.num_nodes
            }
            NumaPolicy::ProducerLocal => current_numa_node(),
        }
    }
}

/// Returns true if the platform supports NUMA memory binding.
#[must_use]
pub fn numa_available() -> bool {
    detect_num_nodes() > 1
}

// =============================================================================
// Linux implementation (real NUMA via mmap + mbind)
// =============================================================================

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::ops::{Deref, DerefMut};

    /// Buffer backed by `mmap`'d memory bound to a NUMA node.
    pub struct NumaBuffer<T> {
        /// Typed pointer into the mmap region.
        ptr: *mut MaybeUninit<T>,
        /// Number of `MaybeUninit<T>` elements.
        len: usize,
        /// Raw mmap base pointer (for munmap).
        mmap_ptr: *mut libc::c_void,
        /// Total mmap size in bytes (for munmap).
        mmap_len: usize,
    }

    // Safety: NumaBuffer owns its mmap allocation exclusively.
    // No other references exist to this memory region.
    unsafe impl<T: Send> Send for NumaBuffer<T> {}

    impl<T> Deref for NumaBuffer<T> {
        type Target = [MaybeUninit<T>];

        fn deref(&self) -> &[MaybeUninit<T>] {
            // Safety: ptr is valid for len elements for the lifetime of the mmap.
            unsafe { std::slice::from_raw_parts(self.ptr, self.len) }
        }
    }

    impl<T> DerefMut for NumaBuffer<T> {
        fn deref_mut(&mut self) -> &mut [MaybeUninit<T>] {
            // Safety: ptr is valid for len elements and we have &mut self.
            unsafe { std::slice::from_raw_parts_mut(self.ptr, self.len) }
        }
    }

    impl<T> Drop for NumaBuffer<T> {
        fn drop(&mut self) {
            // Safety: mmap_ptr and mmap_len were set by a successful mmap call.
            unsafe {
                libc::munmap(self.mmap_ptr, self.mmap_len);
            }
        }
    }

    /// Build a nodemask bitmask for `mbind`. Node `n` is bit `n`.
    fn build_nodemask(node: u16) -> (Vec<libc::c_ulong>, libc::c_ulong) {
        let bits_per_ulong = std::mem::size_of::<libc::c_ulong>() * 8;
        let word_idx = node as usize / bits_per_ulong;
        let bit_idx = node as usize % bits_per_ulong;

        let num_words = word_idx + 1;
        let mut mask = vec![0 as libc::c_ulong; num_words];
        mask[word_idx] = 1 << bit_idx;

        let maxnode = (num_words * bits_per_ulong + 1) as libc::c_ulong;
        (mask, maxnode)
    }

    pub(super) fn allocate_numa<T>(
        capacity: usize,
        node: u16,
        huge_pages: bool,
    ) -> NumaBuffer<T> {
        let elem_size = std::mem::size_of::<MaybeUninit<T>>();
        let align = std::mem::align_of::<MaybeUninit<T>>();
        let total_bytes = elem_size
            .checked_mul(capacity)
            .expect("capacity overflow");

        // Round up to page size for mmap.
        let page_size = unsafe { libc::sysconf(libc::_SC_PAGESIZE) as usize };
        let mmap_len = (total_bytes + page_size - 1) & !(page_size - 1);
        // Also ensure alignment is sufficient for T.
        assert!(
            page_size >= align,
            "page size {} < align_of::<T>() {}",
            page_size,
            align
        );

        let mut flags = libc::MAP_PRIVATE | libc::MAP_ANONYMOUS;
        if huge_pages {
            flags |= libc::MAP_HUGETLB;
        }

        // Safety: mmap with MAP_ANONYMOUS returns zeroed pages.
        // We request mmap_len bytes, which is >= total_bytes.
        let mmap_ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                mmap_len,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                -1,
                0,
            )
        };

        assert!(
            mmap_ptr != libc::MAP_FAILED,
            "mmap failed: {}",
            std::io::Error::last_os_error()
        );

        // Bind memory to the target NUMA node.
        let (nodemask, maxnode) = build_nodemask(node);

        // Safety: mmap_ptr is valid, nodemask is correctly sized.
        // MPOL_BIND = 2 (bind pages to the specified node set).
        let ret = unsafe {
            libc::mbind(
                mmap_ptr,
                mmap_len,
                2, // MPOL_BIND
                nodemask.as_ptr(),
                maxnode as libc::c_ulong,
                0, // flags
            )
        };

        // mbind failure is non-fatal on single-node systems — pages stay
        // on the default node, which is fine.
        if ret != 0 {
            let err = std::io::Error::last_os_error();
            // ENOSYS = kernel without NUMA support, EINVAL = single-node system
            if err.raw_os_error() != Some(libc::ENOSYS)
                && err.raw_os_error() != Some(libc::EINVAL)
            {
                eprintln!(
                    "ringmpsc: mbind to NUMA node {} failed: {} (continuing with default placement)",
                    node, err
                );
            }
        }

        let ptr = mmap_ptr as *mut MaybeUninit<T>;

        NumaBuffer {
            ptr,
            len: capacity,
            mmap_ptr,
            mmap_len,
        }
    }

    pub(super) fn detect_num_nodes_linux() -> u16 {
        // Try reading from /sys/devices/system/node/
        if let Ok(entries) = std::fs::read_dir("/sys/devices/system/node/") {
            let count = entries
                .filter_map(Result::ok)
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .is_some_and(|name| name.starts_with("node"))
                })
                .count();
            if count > 0 {
                return count as u16;
            }
        }
        1 // Single-node fallback
    }

    pub(super) fn current_numa_node_linux() -> u16 {
        // Safety: sched_getcpu is always safe to call.
        let cpu = unsafe { libc::sched_getcpu() };
        if cpu < 0 {
            return 0;
        }

        // Read the NUMA node for this CPU from sysfs.
        let pattern = format!("/sys/devices/system/cpu/cpu{cpu}/node*");
        // Use a simple readdir-based lookup instead of glob.
        let cpu_dir = format!("/sys/devices/system/cpu/cpu{cpu}/");
        if let Ok(entries) = std::fs::read_dir(&cpu_dir) {
            for entry in entries.flatten() {
                if let Some(name) = entry.file_name().to_str() {
                    if let Some(node_str) = name.strip_prefix("node") {
                        if let Ok(node) = node_str.parse::<u16>() {
                            return node;
                        }
                    }
                }
            }
        }

        // Fallback: use get_mempolicy
        let mut node: libc::c_int = 0;
        // Safety: get_mempolicy with NULL nodemask queries current thread policy.
        let ret = unsafe {
            libc::syscall(
                libc::SYS_get_mempolicy,
                &mut node as *mut libc::c_int,
                std::ptr::null_mut::<libc::c_ulong>(),
                0 as libc::c_ulong,
                std::ptr::null_mut::<libc::c_void>(),
                0 as libc::c_ulong,
            )
        };
        if ret == 0 && node >= 0 {
            return node as u16;
        }

        let _ = pattern; // suppress unused warning
        0
    }

    // Safety: NumaAllocator satisfies INV-MEM-04:
    // 1. allocate() returns exactly `capacity` elements (mmap_len >= capacity * elem_size).
    // 2. mmap memory is valid for reads/writes until munmap.
    // 3. Deref/DerefMut produce contiguous slices via from_raw_parts.
    // 4. Drop calls munmap to deallocate.
    unsafe impl BufferAllocator for super::NumaAllocator {
        type Buffer<T> = NumaBuffer<T>;

        fn allocate<T>(&self, capacity: usize) -> NumaBuffer<T> {
            let node = self.resolve_node();
            allocate_numa(capacity, node, self.huge_pages)
        }
    }
}

// =============================================================================
// Non-Linux fallback (delegates to HeapAllocator)
// =============================================================================

#[cfg(not(target_os = "linux"))]
mod platform {
    use super::*;
    use crate::allocator::HeapAllocator;
    use std::sync::Once;

    static WARN_ONCE: Once = Once::new();

    // Safety: Delegates to HeapAllocator which satisfies INV-MEM-04.
    // INV-NUMA-02: On non-NUMA platforms, produces a valid buffer.
    unsafe impl BufferAllocator for super::NumaAllocator {
        type Buffer<T> = Box<[MaybeUninit<T>]>;

        fn allocate<T>(&self, capacity: usize) -> Box<[MaybeUninit<T>]> {
            WARN_ONCE.call_once(|| {
                eprintln!(
                    "ringmpsc: NUMA allocation requested but not available on this platform. \
                     Falling back to heap allocation."
                );
            });
            let buffer = HeapAllocator.allocate(capacity);
            crate::invariants::debug_assert_numa_fallback!(buffer, capacity);
            buffer
        }
    }
}

// =============================================================================
// Platform-agnostic helpers
// =============================================================================

fn detect_num_nodes() -> u16 {
    #[cfg(target_os = "linux")]
    {
        platform::detect_num_nodes_linux()
    }
    #[cfg(not(target_os = "linux"))]
    {
        1
    }
}

#[allow(dead_code)] // used in resolve_node (Linux path)
fn current_numa_node() -> u16 {
    #[cfg(target_os = "linux")]
    {
        platform::current_numa_node_linux()
    }
    #[cfg(not(target_os = "linux"))]
    {
        0
    }
}
