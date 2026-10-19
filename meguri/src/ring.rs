//! Core Ring struct.
//!
//! Owns the io_uring fd, mmap'd SQ/CQ regions, and the dispatcher.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::ptr;
use std::time::{Duration, Instant};

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
        // SAFETY: a fresh shared mapping chosen by the kernel (null hint), so
        // it cannot overlap memory Rust already uses; the caller guarantees
        // `ring_fd`/`offset` name an io_uring region. The result is checked
        // against MAP_FAILED before use.
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
        // SAFETY: `ptr`/`len` are exactly what mmap returned for this region,
        // unmapped once, here. The only pointers into it live in the `Ring`
        // that owns this region (its SQ/CQ), which is being dropped and does
        // not use them afterwards. The result is ignored on purpose: munmap
        // only fails for invalid arguments (a bug here), and a destructor
        // has no way to recover; the mapping would merely stay in place.
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
    // Field order is drop order. The ring fd closes first; the reclaim pool
    // is freed only after `Drop::drop` has drained in-flight operations; the
    // mmaps go last (the kernel keeps its own reference to the ring memory,
    // so unmapping after close is fine).
    ring_fd: OwnedFd,
    sq: SubmissionQueue,
    cq: CompletionQueue,
    dispatcher: Dispatcher,
    reclaim: ReclaimPool,
    eventfd: Option<RawFd>,
    params: IoUringParams,
    /// Timespec for the timer `drain_in_flight` arms; boxed so its address
    /// survives the ring being moved before that SQE is submitted.
    drain_ts: Box<libc::timespec>,

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

        // SAFETY: `params` is a live, exclusively borrowed `io_uring_params`
        // (repr(C), kernel layout) with only `flags` set and every reserved
        // field zero, as the kernel requires; it writes the offsets back into
        // it synchronously. The returned fd is owned right below.
        let raw_fd = unsafe { io_uring_setup(entries, &mut params)? };
        // Own the fd at once so the `?` on every mmap below closes it instead
        // of leaking it.
        // SAFETY: io_uring_setup just returned this fd; nothing else owns it.
        let owned_fd = unsafe { OwnedFd::from_raw_fd(raw_fd) };
        let ring_fd = owned_fd.as_raw_fd();

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

        // SAFETY (all three maps): `ring_fd` is the io_uring fd just created
        // (kept open by `owned_fd`), the offsets are the kernel-defined
        // region constants, and the lengths are computed from the
        // kernel-reported `params` exactly as liburing does.
        let sq_ring_mmap = unsafe { MmapRegion::map(ring_fd, sq_mmap_len, IORING_OFF_SQ_RING)? };

        let cq_ring_mmap = if single_mmap {
            None
        } else {
            // SAFETY: see above.
            Some(unsafe { MmapRegion::map(ring_fd, cq_ring_size, IORING_OFF_CQ_RING)? })
        };

        let cq_ring_base = if single_mmap {
            sq_ring_mmap.as_ptr()
        } else {
            cq_ring_mmap.as_ref().unwrap().as_ptr()
        };

        // SAFETY: see above.
        let sqes_mmap = unsafe { MmapRegion::map(ring_fd, sqes_size, IORING_OFF_SQES)? };

        // Build SQ and CQ from the mmap'd regions.
        // SAFETY: the pointers are the bases of the SQ ring and SQE mappings
        // of the ring `params` describes. Both mappings move into the
        // returned `Ring` next to the queues, so they outlive them (see the
        // queues' "Pointer validity" docs).
        let sq = unsafe {
            SubmissionQueue::from_raw(
                sq_ring_mmap.as_ptr(),
                sqes_mmap.as_mut_ptr() as *mut IoUringSqe,
                &params,
            )
        };
        // SAFETY: `cq_ring_base` is the CQ ring (inside the SQ mapping with
        // FEAT_SINGLE_MMAP, else its own mapping), kept alive by `Ring` as
        // above.
        let cq = unsafe { CompletionQueue::from_raw(cq_ring_base, &params) };

        Ok(Self {
            ring_fd: owned_fd,
            sq,
            cq,
            dispatcher: Dispatcher::new(),
            reclaim: ReclaimPool::new(),
            eventfd: None,
            params,
            _sq_ring_mmap: sq_ring_mmap,
            _cq_ring_mmap: cq_ring_mmap,
            drain_ts: Box::new(libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            }),
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
    ///
    /// The ring does not take ownership of `fd` and never closes it; the
    /// kernel holds its own reference to the eventfd while it is registered.
    pub fn register_eventfd(&mut self, fd: RawFd) -> io::Result<()> {
        // SAFETY: IORING_REGISTER_EVENTFD reads one `int` from `arg`, here a
        // local that lives across the (synchronous) call; `nr_args` is 1.
        // An invalid `fd` is reported as an error, not UB.
        unsafe {
            crate::syscall::io_uring_register(
                self.ring_fd.as_raw_fd(),
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
        // SAFETY: `ring_fd` is this ring's open fd; `pending` counts SQEs
        // published by `flush` (each built by a builder whose own safety
        // contract covers the memory it references); no sigmask.
        unsafe { io_uring_enter(self.ring_fd.as_raw_fd(), pending, 0, 0, ptr::null(), 0) }
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
        // SAFETY: as in `submit`.
        unsafe {
            io_uring_enter(
                self.ring_fd.as_raw_fd(),
                pending,
                min_complete,
                flags,
                ptr::null(),
                0,
            )
        }
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

    /// Release an operation whose future was dropped.
    ///
    /// If the kernel is still working on it, `resources` move to the reclaim
    /// pool and an `ASYNC_CANCEL` is queued so the kernel stops early (an idle
    /// `accept` or `recv` would otherwise hold its buffers until the ring is
    /// destroyed). The cancel's own completion is discarded.
    pub(crate) fn release_op(
        &mut self,
        user_data: u64,
        resources: impl crate::reclaim::Reclaimable,
    ) {
        if self
            .dispatcher
            .release(user_data, resources, &mut self.reclaim)
        {
            let cancel_ud = self.dispatcher.register_internal();
            if !self.sq.push_cancel(cancel_ud, user_data) {
                // SQ full: the operation still completes on its own; the
                // resources are reclaimed then.
                self.dispatcher.unregister(cancel_ud);
            }
        }
    }

    /// The kernel-reported parameters for this ring.
    pub fn params(&self) -> &IoUringParams {
        &self.params
    }
}

impl Ring {
    /// Cancel every operation the dispatcher still tracks and wait up to
    /// `timeout` for their completions. Returns `true` when nothing tracked by
    /// the dispatcher is in flight any more.
    ///
    /// Closing the ring fd does not stop the kernel synchronously: the
    /// kernel cancels outstanding requests later, from a workqueue, and until
    /// then a pending `recv`/`read` can still write into its buffer. So the
    /// buffers in the reclaim pool must outlive the requests, not the fd.
    ///
    /// Operations pushed directly through [`Ring::sq`] without registering
    /// with the dispatcher are invisible here; their owner must drain them
    /// before dropping the ring.
    pub(crate) fn drain_in_flight(&mut self, timeout: Duration) -> bool {
        self.poll_completions();
        if self.dispatcher.in_flight() == 0 {
            return true;
        }

        // `release_op` normally queued a cancel already, but it may have
        // found the SQ full. Cancelling twice is harmless (-ENOENT/-EALREADY).
        for target in self.dispatcher.cancellable_user_data() {
            let cancel_ud = self.dispatcher.register_internal();
            if !self.sq.push_cancel(cancel_ud, target) {
                let _ = self.submit();
                if !self.sq.push_cancel(cancel_ud, target) {
                    self.dispatcher.unregister(cancel_ud);
                }
            }
        }

        // A timer bounds the blocking wait below.
        *self.drain_ts = libc::timespec {
            tv_sec: timeout.as_secs() as libc::time_t,
            tv_nsec: timeout.subsec_nanos().into(),
        };
        let ts: *const libc::timespec = &raw const *self.drain_ts;
        let timer_ud = self.dispatcher.register_internal();
        // SAFETY: `ts` points into a box owned by the ring, so it stays valid
        // and unmoved for as long as the SQE can be submitted, even if this
        // function returns before submitting it. A later `drain_in_flight`
        // rewriting it only changes the duration a not-yet-submitted timer
        // would read; the kernel copies it at submission.
        let pushed = unsafe { self.sq.push_timeout(timer_ud, ts, 0, 0) } || {
            let _ = self.submit();
            // SAFETY: as above.
            unsafe { self.sq.push_timeout(timer_ud, ts, 0, 0) }
        };
        if !pushed {
            self.dispatcher.unregister(timer_ud);
            return false;
        }

        let deadline = Instant::now() + timeout;
        loop {
            self.poll_completions();
            let timer_pending = usize::from(self.dispatcher.is_pending(timer_ud));
            if self.dispatcher.in_flight() <= timer_pending {
                if timer_pending == 1 {
                    // Stop the timer too; its completion is discarded.
                    let cancel_ud = self.dispatcher.register_internal();
                    if !self.sq.push_cancel(cancel_ud, timer_ud) {
                        self.dispatcher.unregister(cancel_ud);
                    }
                    let _ = self.submit();
                }
                return true;
            }
            if timer_pending == 0 || Instant::now() >= deadline {
                return false;
            }
            match self.submit_and_wait(1) {
                Ok(_) => {}
                Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => return false,
            }
        }
    }
}

impl AsRawFd for Ring {
    fn as_raw_fd(&self) -> RawFd {
        self.ring_fd.as_raw_fd()
    }
}

/// How long `Ring::drop` waits for cancelled operations to complete.
const DROP_DRAIN_TIMEOUT: Duration = Duration::from_secs(1);

impl Drop for Ring {
    fn drop(&mut self) {
        // Free operation buffers only once the kernel has finished with
        // them. If some operation outlives the wait, leak the reclaim pool:
        // a leak is safe, freeing memory the kernel may still write is not.
        if !self.drain_in_flight(DROP_DRAIN_TIMEOUT) {
            tracing::warn!(
                in_flight = self.dispatcher.in_flight(),
                "meguri ring dropped with operations still in flight; leaking their buffers"
            );
            std::mem::forget(std::mem::take(&mut self.reclaim));
        }
        // The ring fd (`OwnedFd`) and the mmaps close in their own drops.
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

    /// A pure timer (count 0) must fire with -ETIME, not be rejected.
    #[test]
    fn timeout_fires_with_etime() {
        let mut ring = Ring::new(8).expect("failed to create ring");
        let ts = libc::timespec {
            tv_sec: 0,
            tv_nsec: 1_000_000,
        };

        // SAFETY: `ts` outlives the submit_and_wait below, which submits it.
        assert!(unsafe { ring.sq().push_timeout(7, &ts, 0, 0) });
        ring.submit_and_wait(1).expect("submit_and_wait failed");

        let cqe = ring.cq.peek().expect("expected one CQE");
        assert_eq!(cqe.user_data, 7);
        assert_eq!(cqe.res, -libc::ETIME, "timeout SQE rejected or misfired");
        ring.cq.advance();
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
