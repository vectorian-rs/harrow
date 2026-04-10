//! Core Ring struct.
//!
//! Owns the io_uring fd, mmap'd SQ/CQ regions, and the dispatcher.

use std::io;
use std::os::fd::{AsRawFd, RawFd};
use std::ptr;

use crate::cq::CompletionQueue;
use crate::dispatcher::Dispatcher;
use crate::reclaim::ReclaimPool;
use crate::sq::SubmissionQueue;
use crate::syscall::{
    IORING_ENTER_GETEVENTS, IORING_FEAT_EXT_ARG, IORING_FEAT_SINGLE_MMAP, IORING_OFF_CQ_RING,
    IORING_OFF_SQ_RING, IORING_OFF_SQES, IORING_SETUP_COOP_TASKRUN, IORING_SETUP_SINGLE_ISSUER,
    IORING_SETUP_SUBMIT_ALL, IoUringCqe, IoUringParams, IoUringSqe, io_uring_enter, io_uring_setup,
};

/// An mmap'd region from the io_uring ring fd.
struct MmapRegion {
    ptr: *mut u8,
    len: usize,
}

impl MmapRegion {
    /// Map a region from the ring fd at the given offset.
    ///
    /// # Safety
    /// `ring_fd` must be a valid io_uring fd. `offset` must be a valid
    /// io_uring mmap offset (IORING_OFF_SQ_RING, IORING_OFF_CQ_RING, or
    /// IORING_OFF_SQES).
    unsafe fn map(ring_fd: RawFd, len: usize, offset: u64) -> io::Result<Self> {
        let ptr = unsafe {
            libc::mmap(
                ptr::null_mut(),
                len,
                libc::PROT_READ | libc::PROT_WRITE,
                libc::MAP_SHARED | libc::MAP_POPULATE,
                ring_fd,
                offset as libc::off_t,
            )
        };
        if ptr == libc::MAP_FAILED {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self {
                ptr: ptr as *mut u8,
                len,
            })
        }
    }

    fn as_ptr(&self) -> *const u8 {
        self.ptr
    }

    fn as_mut_ptr(&self) -> *mut u8 {
        self.ptr
    }
}

impl Drop for MmapRegion {
    fn drop(&mut self) {
        unsafe {
            libc::munmap(self.ptr as *mut _, self.len);
        }
    }
}

/// The core io_uring ring.
///
/// Owns the ring fd, mmap'd SQ/CQ regions, the dispatcher for waker
/// management, and the reclaim pool for dropped operations.
pub struct Ring {
    ring_fd: RawFd,
    sq: SubmissionQueue,
    cq: CompletionQueue,
    dispatcher: Dispatcher,
    reclaim: ReclaimPool,
    eventfd: Option<RawFd>,
    params: IoUringParams,

    // Hold mmap regions so they live as long as the Ring. The SQ, CQ, and
    // SQE pointers point into these regions.
    _sq_ring_mmap: MmapRegion,
    _cq_ring_mmap: Option<MmapRegion>,
    _sqes_mmap: MmapRegion,
}

impl Ring {
    /// Create a new io_uring ring with the given number of entries.
    ///
    /// `entries` should be a power of 2. The kernel may clamp non-power-of-2
    /// values to the nearest power of 2.
    ///
    /// # Features enabled
    ///
    /// - `IORING_SETUP_COOP_TASKRUN` — cooperative task running (reduces IPIs).
    /// - `IORING_SETUP_SINGLE_ISSUER` — optimize for single-threaded usage.
    /// - `IORING_SETUP_SUBMIT_ALL` — continue submitting even if one SQE fails.
    pub fn new(entries: u32) -> io::Result<Self> {
        let params = IoUringParams {
            flags: IORING_SETUP_COOP_TASKRUN | IORING_SETUP_SINGLE_ISSUER | IORING_SETUP_SUBMIT_ALL,
            ..IoUringParams::default()
        };
        // io_uring_setup writes back into params, so we need a mutable copy.
        let mut params = params;

        // SAFETY: io_uring_setup is safe with a valid params pointer.
        let ring_fd = unsafe { io_uring_setup(entries, &mut params)? };

        // Calculate mmap sizes.
        let sq_ring_size =
            params.sq_off.array as usize + params.sq_entries as usize * std::mem::size_of::<u32>();
        let cq_ring_size = params.cq_off.cqes as usize
            + params.cq_entries as usize * std::mem::size_of::<IoUringCqe>();
        let sqes_size = params.sq_entries as usize * std::mem::size_of::<IoUringSqe>();

        // If FEAT_SINGLE_MMAP, SQ and CQ rings share a single mapping.
        let single_mmap = params.features & IORING_FEAT_SINGLE_MMAP != 0;
        let sq_mmap_len = if single_mmap {
            std::cmp::max(sq_ring_size, cq_ring_size)
        } else {
            sq_ring_size
        };

        // SAFETY: ring_fd is valid, offsets are kernel-defined constants.
        let sq_ring_mmap = unsafe { MmapRegion::map(ring_fd, sq_mmap_len, IORING_OFF_SQ_RING)? };

        let cq_ring_mmap = if single_mmap {
            None
        } else {
            Some(unsafe { MmapRegion::map(ring_fd, cq_ring_size, IORING_OFF_CQ_RING)? })
        };

        let cq_ring_base = if single_mmap {
            sq_ring_mmap.as_ptr()
        } else {
            cq_ring_mmap.as_ref().unwrap().as_ptr()
        };

        let sqes_mmap = unsafe { MmapRegion::map(ring_fd, sqes_size, IORING_OFF_SQES)? };

        // Build SQ and CQ from the mmap'd regions.
        let sq = unsafe {
            SubmissionQueue::from_raw(
                sq_ring_mmap.as_ptr(),
                sqes_mmap.as_mut_ptr() as *mut IoUringSqe,
                &params,
            )
        };
        let cq = unsafe { CompletionQueue::from_raw(cq_ring_base, &params) };

        Ok(Self {
            ring_fd,
            sq,
            cq,
            dispatcher: Dispatcher::new(),
            reclaim: ReclaimPool::new(),
            eventfd: None,
            params,
            _sq_ring_mmap: sq_ring_mmap,
            _cq_ring_mmap: cq_ring_mmap,
            _sqes_mmap: sqes_mmap,
        })
    }

    /// Check if the kernel supports `IORING_FEAT_EXT_ARG` (5.11+).
    pub fn has_ext_arg(&self) -> bool {
        self.params.features & IORING_FEAT_EXT_ARG != 0
    }

    /// Check if the kernel supports single mmap (SQ+CQ in one region).
    pub fn has_single_mmap(&self) -> bool {
        self.params.features & IORING_FEAT_SINGLE_MMAP != 0
    }

    /// Register an eventfd for cross-thread wakeup.
    pub fn register_eventfd(&mut self, fd: RawFd) -> io::Result<()> {
        unsafe {
            crate::syscall::io_uring_register(
                self.ring_fd,
                crate::syscall::IORING_REGISTER_EVENTFD,
                &fd as *const _ as *const _,
                1,
            )?;
        }
        self.eventfd = Some(fd);
        Ok(())
    }

    /// Submit pending SQEs to the kernel.
    pub fn submit(&mut self) -> io::Result<u32> {
        let pending = self.sq.flush();
        if pending == 0 {
            return Ok(0);
        }
        unsafe { io_uring_enter(self.ring_fd, pending, 0, 0, ptr::null(), 0) }
    }

    /// Submit pending SQEs and wait for at least `min_complete` CQEs.
    pub fn submit_and_wait(&mut self, min_complete: u32) -> io::Result<u32> {
        let pending = self.sq.flush();
        if pending == 0 && min_complete == 0 {
            return Ok(0);
        }
        let flags = if min_complete > 0 {
            IORING_ENTER_GETEVENTS
        } else {
            0
        };
        unsafe { io_uring_enter(self.ring_fd, pending, min_complete, flags, ptr::null(), 0) }
    }

    /// Poll the completion queue, dispatching completions to wakers and
    /// reclaim pools. Returns the number of completions processed.
    pub fn poll_completions(&mut self) -> usize {
        let mut count = 0;
        while let Some(cqe) = self.cq.peek() {
            let user_data = cqe.user_data;
            let res = cqe.res;
            let flags = cqe.flags;

            self.dispatcher
                .complete(user_data, res, flags, &mut self.reclaim);
            self.cq.advance();
            count += 1;
        }
        if count > 0 {
            self.cq.flush_head();
        }
        count
    }

    /// Submit a single SQE and wait for its completion.
    pub fn submit_one_and_wait(&mut self) -> io::Result<u32> {
        self.submit_and_wait(1)
    }

    /// Access the submission queue for building SQEs.
    pub fn sq(&mut self) -> &mut SubmissionQueue {
        &mut self.sq
    }

    /// Access the completion queue.
    pub fn cq(&self) -> &CompletionQueue {
        &self.cq
    }

    /// Access the completion queue mutably.
    pub fn cq_mut(&mut self) -> &mut CompletionQueue {
        &mut self.cq
    }

    /// Access the dispatcher for registering wakers.
    pub fn dispatcher(&mut self) -> &mut Dispatcher {
        &mut self.dispatcher
    }

    /// Access the reclaim pool.
    pub fn reclaim(&mut self) -> &mut ReclaimPool {
        &mut self.reclaim
    }

    /// The kernel-reported parameters for this ring.
    pub fn params(&self) -> &IoUringParams {
        &self.params
    }
}

impl AsRawFd for Ring {
    fn as_raw_fd(&self) -> RawFd {
        self.ring_fd
    }
}

impl Drop for Ring {
    fn drop(&mut self) {
        // Drain remaining completions to reclaim buffers.
        self.poll_completions();
        // Close the ring fd. The kernel cleans up resources.
        // Mmap regions are unmapped by their own Drop impls.
        unsafe {
            libc::close(self.ring_fd);
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Verify that a NOP SQE can be submitted and completed.
    /// This is the Phase 1 gate test — proves the mmap'd ring works end-to-end.
    #[test]
    fn nop_roundtrip() {
        let mut ring = Ring::new(32).expect("failed to create ring");

        // Push a NOP with a known user_data.
        let user_data = 42u64;
        assert!(ring.sq().push_nop(user_data), "SQ should not be full");

        // Submit and wait for one completion.
        ring.submit_and_wait(1).expect("submit_and_wait failed");

        // Poll the CQE.
        let cqe = ring.cq.peek().expect("expected one CQE");
        assert_eq!(cqe.user_data, user_data);
        assert_eq!(cqe.res, 0, "NOP should complete with res=0");
        ring.cq.advance();

        // No more CQEs.
        assert!(ring.cq.peek().is_none());
    }

    /// Submit multiple NOPs and verify all complete.
    #[test]
    fn multiple_nops() {
        let mut ring = Ring::new(32).expect("failed to create ring");

        let count = 8u64;
        for i in 0..count {
            assert!(ring.sq().push_nop(i + 1));
        }

        ring.submit_and_wait(count as u32)
            .expect("submit_and_wait failed");

        let completed = ring.poll_completions();
        assert_eq!(completed, count as usize);
    }

    /// Verify SQE and CQE struct sizes match kernel expectations.
    #[test]
    fn struct_sizes() {
        assert_eq!(
            std::mem::size_of::<IoUringSqe>(),
            64,
            "IoUringSqe must be 64 bytes"
        );
        assert_eq!(
            std::mem::size_of::<IoUringCqe>(),
            16,
            "IoUringCqe must be 16 bytes"
        );
    }
}
