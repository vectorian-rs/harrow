//! Completion queue management.
//!
//! Wraps the kernel-mapped CQ ring and CQE array. CQEs are read from here
//! after `io_uring_enter` returns.

use std::sync::atomic::{AtomicU32, Ordering};

use crate::syscall::{IoUringCqe, IoUringParams};

/// The completion queue.
///
/// Holds pointers into the kernel-mapped CQ ring (head, tail, mask) and the
/// CQE array. Atomic orderings:
///
/// - **tail** (kernel-written, user-read): `Acquire`
/// - **head** (user-written, kernel-read): `Release`
///
/// # Pointer validity
///
/// All pointers point into the CQ ring mapping owned by the [`Ring`] that
/// created this queue (`Ring::_sq_ring_mmap` or `_cq_ring_mmap`). The queue
/// is a field of that `Ring` and only reachable through it, so the mapping
/// outlives every use of the pointers below.
///
/// [`Ring`]: crate::Ring
pub struct CompletionQueue {
    // Pointers into the kernel-mapped CQ ring region.
    head: *const AtomicU32,
    tail: *const AtomicU32,
    ring_mask: u32,
    cqes: *const IoUringCqe,

    // Local head — we advance this as we consume CQEs.
    cq_head: u32,
}

// SAFETY: the raw pointers target shared kernel memory that is not tied to
// any thread, and every access through them is either atomic (head, tail) or
// a read of a CQE the kernel has published and will not rewrite until the
// head moves past it. Moving the queue to another thread changes none of
// that; it is not `Sync`, and all mutation needs `&mut self`.
unsafe impl Send for CompletionQueue {}

impl CompletionQueue {
    /// Create a CQ from the kernel-mapped ring region.
    ///
    /// # Safety
    /// - `cq_ring_base` must point to a valid mmap'd CQ ring region.
    /// - `params` must describe the ring that produced this mapping.
    pub unsafe fn from_raw(cq_ring_base: *const u8, params: &IoUringParams) -> Self {
        // SAFETY: per the caller's contract the mapping is the CQ ring of the
        // ring `params` describes, so the kernel-provided `cq_off` offsets
        // are in bounds and suitably aligned: `head`, `tail` and `ring_mask`
        // are `u32`s (head/tail accessed atomically, as the kernel does), and
        // `cqes` is the start of the `cq_entries`-long CQE array.
        unsafe {
            let head = cq_ring_base.add(params.cq_off.head as usize) as *const AtomicU32;
            let tail = cq_ring_base.add(params.cq_off.tail as usize) as *const AtomicU32;
            let ring_mask = *(cq_ring_base.add(params.cq_off.ring_mask as usize) as *const u32);
            let cqes = cq_ring_base.add(params.cq_off.cqes as usize) as *const IoUringCqe;

            // Read the current kernel head so we stay in sync.
            let cq_head = (*head).load(Ordering::Acquire);

            Self {
                head,
                tail,
                ring_mask,
                cqes,
                cq_head,
            }
        }
    }

    /// Peek at the next CQE without advancing the head.
    ///
    /// Returns `None` if no completions are available. Reads the kernel tail
    /// with `Acquire` ordering to see newly posted completions.
    pub fn peek(&self) -> Option<&IoUringCqe> {
        // SAFETY: `tail` points to the kernel's CQ tail (see "Pointer
        // validity"), an aligned u32 the kernel only updates atomically.
        let tail = unsafe { (*self.tail).load(Ordering::Acquire) };
        if self.cq_head == tail {
            return None;
        }
        let idx = self.cq_head & self.ring_mask;
        // SAFETY: `idx <= ring_mask < cq_entries`, so the slot is inside the
        // CQE array. `cq_head != tail`, and the Acquire load above makes the
        // kernel's write of this CQE visible. The kernel does not reuse the
        // slot until the published head moves past it, which needs
        // `advance(&mut self)`: impossible while the returned borrow of
        // `self` is alive (`flush_head` publishes `cq_head`, still this slot).
        Some(unsafe { &*self.cqes.add(idx as usize) })
    }

    /// Advance the local head by one, consuming the current CQE.
    ///
    /// This only updates the local counter. Call `flush_head()` after
    /// processing a batch to publish the new head to the kernel.
    pub fn advance(&mut self) {
        self.cq_head = self.cq_head.wrapping_add(1);
    }

    /// Publish the local head to kernel-mapped memory.
    ///
    /// This issues a single `Release` store, allowing the kernel to reuse
    /// all CQE slots up to the new head. Call after processing a batch of
    /// CQEs rather than after each individual CQE.
    pub fn flush_head(&self) {
        // SAFETY: `head` points to the kernel's CQ head (see "Pointer
        // validity"); userspace is its only writer, and Release orders our
        // reads of the consumed CQEs before the kernel may overwrite them.
        unsafe { (*self.head).store(self.cq_head, Ordering::Release) };
    }

    /// Number of CQEs available for reading.
    pub fn ready(&self) -> u32 {
        // SAFETY: as for the tail load in `peek`.
        let tail = unsafe { (*self.tail).load(Ordering::Acquire) };
        tail.wrapping_sub(self.cq_head)
    }
}
