//! Submission queue management.
//!
//! Wraps the kernel-mapped SQ ring and SQE array. SQEs are built here and
//! submitted to the kernel via `Ring::submit()`.

use std::sync::atomic::{AtomicU32, Ordering};

use crate::syscall::{
    IORING_OP_ACCEPT, IORING_OP_ASYNC_CANCEL, IORING_OP_CLOSE, IORING_OP_CONNECT,
    IORING_OP_LINK_TIMEOUT, IORING_OP_NOP, IORING_OP_READ, IORING_OP_RECV, IORING_OP_SEND,
    IORING_OP_TIMEOUT, IORING_OP_WRITE, IOSQE_IO_LINK, IoUringParams, IoUringSqe,
};

/// The submission queue.
///
/// Holds pointers into the kernel-mapped SQ ring (head, tail, mask, flags,
/// array) and the separately-mapped SQE array. All shared-memory accesses use
/// the correct atomic orderings:
///
/// - **head** (kernel-written, user-read): `Acquire`
/// - **tail** (user-written, kernel-read): `Release`
pub struct SubmissionQueue {
    // Pointers into the kernel-mapped SQ ring region.
    head: *const AtomicU32,
    tail: *const AtomicU32,
    ring_mask: u32,
    ring_entries: u32,
    flags: *const AtomicU32,

    // Pointer to the separately-mapped SQE array.
    sqes: *mut IoUringSqe,

    // Local tail — we write SQEs at this position and flush to kernel on submit.
    sq_tail: u32,
}

// SAFETY: SQ is accessed only from the ring owner's thread (SINGLE_ISSUER).
unsafe impl Send for SubmissionQueue {}

impl SubmissionQueue {
    /// Create an SQ from the kernel-mapped ring region and SQE array.
    ///
    /// # Safety
    /// - `sq_ring_base` must point to a valid mmap'd SQ ring region.
    /// - `sqes` must point to a valid mmap'd SQE array.
    /// - `params` must describe the ring that produced these mappings.
    pub unsafe fn from_raw(
        sq_ring_base: *const u8,
        sqes: *mut IoUringSqe,
        params: &IoUringParams,
    ) -> Self {
        unsafe {
            let head = sq_ring_base.add(params.sq_off.head as usize) as *const AtomicU32;
            let tail = sq_ring_base.add(params.sq_off.tail as usize) as *const AtomicU32;
            let ring_mask = *(sq_ring_base.add(params.sq_off.ring_mask as usize) as *const u32);
            let ring_entries =
                *(sq_ring_base.add(params.sq_off.ring_entries as usize) as *const u32);
            let flags = sq_ring_base.add(params.sq_off.flags as usize) as *const AtomicU32;
            let array = sq_ring_base.add(params.sq_off.array as usize) as *mut u32;

            // Initialize the SQ array with direct mapping: array[i] = i.
            // Ring position i always maps to SQE index i, so the array is
            // never touched again and needs no field.
            for i in 0..ring_entries {
                *array.add(i as usize) = i;
            }

            // Read the current kernel tail so we stay in sync.
            let sq_tail = (*tail).load(Ordering::Acquire);

            Self {
                head,
                tail,
                ring_mask,
                ring_entries,
                flags,
                sqes,
                sq_tail,
            }
        }
    }

    /// Claim and zero the next free SQE slot, or `None` if the SQ is full.
    fn slot(&mut self) -> Option<&mut IoUringSqe> {
        let head = self.load_head();
        if self.sq_tail.wrapping_sub(head) >= self.ring_entries {
            return None; // SQ is full
        }
        let idx = self.sq_tail & self.ring_mask;
        // SAFETY: `idx <= ring_mask < ring_entries`, the length of the SQE
        // array mapped by `Ring` (which outlives `self`). The slot lies
        // outside `head..tail` as published to the kernel, so the kernel is
        // not reading it, and `&mut self` makes this the only reference.
        let sqe = unsafe { &mut *self.sqes.add(idx as usize) };
        // Zero the SQE so callers start from a clean slate.
        *sqe = IoUringSqe::zeroed();
        self.sq_tail = self.sq_tail.wrapping_add(1);
        Some(sqe)
    }

    /// Get a mutable reference to the next available SQE slot.
    ///
    /// Returns `None` if the SQ is full (local tail has caught up with
    /// the kernel head by `ring_entries`).
    ///
    /// # Safety
    ///
    /// The SQE is handed to the kernel on the next submit as written. Every
    /// address in it (`addr`, `off` for accept/connect, `addr3`, ...) must
    /// stay valid, with the access the opcode implies, and unmoved until
    /// the operation's CQE has been consumed; every fd must be owned by the
    /// caller for the duration. The `push_*` builders state the same
    /// contract per opcode.
    pub unsafe fn next_sqe(&mut self) -> Option<&mut IoUringSqe> {
        self.slot()
    }

    /// Push a pre-built SQE into the next available slot.
    /// Returns `true` if pushed, `false` if the SQ is full.
    ///
    /// # Safety
    ///
    /// As for [`next_sqe`](Self::next_sqe): whatever `sqe` points to must
    /// stay valid until its CQE has been consumed.
    pub unsafe fn push(&mut self, sqe: &IoUringSqe) -> bool {
        let Some(slot) = self.slot() else {
            return false;
        };
        *slot = *sqe;
        true
    }

    /// Flush the local tail to the kernel-mapped SQ tail pointer.
    ///
    /// Must be called before `io_uring_enter`. Returns the number of
    /// entries pending in the ring (submitted but not yet consumed by kernel).
    pub fn flush(&mut self) -> u32 {
        unsafe { (*self.tail).store(self.sq_tail, Ordering::Release) };
        let head = self.load_head();
        self.sq_tail.wrapping_sub(head)
    }

    /// Read the SQ head from kernel-mapped memory.
    fn load_head(&self) -> u32 {
        unsafe { (*self.head).load(Ordering::Acquire) }
    }

    /// Read the SQ flags from kernel-mapped memory.
    pub fn sq_flags(&self) -> u32 {
        unsafe { (*self.flags).load(Ordering::Relaxed) }
    }

    /// Check if the kernel's SQ polling thread needs a wakeup.
    /// Only relevant when `IORING_SETUP_SQPOLL` is active.
    pub fn needs_wakeup(&self) -> bool {
        self.sq_flags() & crate::syscall::IORING_SQ_NEED_WAKEUP != 0
    }

    // -----------------------------------------------------------------------
    // Convenience builders — write directly to the mmap'd SQE slot
    // -----------------------------------------------------------------------

    /// Build and push a `Nop` SQE (useful for testing the ring).
    pub fn push_nop(&mut self, user_data: u64) -> bool {
        let Some(sqe) = self.slot() else {
            return false;
        };
        sqe.opcode = IORING_OP_NOP;
        sqe.fd = -1;
        sqe.user_data = user_data;
        true
    }

    /// Build and push a `Read` SQE.
    ///
    /// # Safety
    ///
    /// `buf..buf + len` must be valid for writes and must neither move, be
    /// freed, nor be accessed by anyone else until the CQE has been consumed:
    /// the kernel writes into it asynchronously. `fd` must stay open (a
    /// closed fd is only `EBADF`, but a reused fd number reads the wrong file).
    pub unsafe fn push_read(
        &mut self,
        user_data: u64,
        fd: i32,
        buf: *mut u8,
        len: u32,
        offset: u64,
    ) -> bool {
        let Some(sqe) = self.slot() else {
            return false;
        };
        sqe.opcode = IORING_OP_READ;
        sqe.fd = fd;
        sqe.off = offset;
        sqe.addr = buf as u64;
        sqe.len = len;
        sqe.user_data = user_data;
        true
    }

    /// Build and push a `Write` SQE.
    ///
    /// # Safety
    ///
    /// `buf..buf + len` must be valid for reads and must neither move, be
    /// freed, nor be modified until the CQE has been consumed: the kernel
    /// reads it asynchronously.
    pub unsafe fn push_write(
        &mut self,
        user_data: u64,
        fd: i32,
        buf: *const u8,
        len: u32,
        offset: u64,
    ) -> bool {
        let Some(sqe) = self.slot() else {
            return false;
        };
        sqe.opcode = IORING_OP_WRITE;
        sqe.fd = fd;
        sqe.off = offset;
        sqe.addr = buf as u64;
        sqe.len = len;
        sqe.user_data = user_data;
        true
    }

    /// Build and push a `Send` SQE.
    ///
    /// # Safety
    ///
    /// As for [`push_write`](Self::push_write).
    pub unsafe fn push_send(
        &mut self,
        user_data: u64,
        fd: i32,
        buf: *const u8,
        len: u32,
        flags: u32,
    ) -> bool {
        let Some(sqe) = self.slot() else {
            return false;
        };
        sqe.opcode = IORING_OP_SEND;
        sqe.fd = fd;
        sqe.addr = buf as u64;
        sqe.len = len;
        sqe.rw_flags = flags;
        sqe.user_data = user_data;
        true
    }

    /// Build and push a `Recv` SQE.
    ///
    /// # Safety
    ///
    /// As for [`push_read`](Self::push_read).
    pub unsafe fn push_recv(
        &mut self,
        user_data: u64,
        fd: i32,
        buf: *mut u8,
        len: u32,
        flags: u32,
    ) -> bool {
        let Some(sqe) = self.slot() else {
            return false;
        };
        sqe.opcode = IORING_OP_RECV;
        sqe.fd = fd;
        sqe.addr = buf as u64;
        sqe.len = len;
        sqe.rw_flags = flags;
        sqe.user_data = user_data;
        true
    }

    /// Build and push an `Accept` SQE.
    ///
    /// # Safety
    ///
    /// `addr` and `addrlen` must both be null, or `addrlen` must point to a
    /// `socklen_t` holding the size of the buffer at `addr`; both must stay
    /// valid and unmoved until the CQE has been consumed (the kernel writes
    /// the peer address when a connection arrives). The accepted fd in the
    /// CQE is owned by whoever consumes the CQE.
    pub unsafe fn push_accept(
        &mut self,
        user_data: u64,
        fd: i32,
        addr: *mut libc::sockaddr,
        addrlen: *mut libc::socklen_t,
        flags: u32,
    ) -> bool {
        let Some(sqe) = self.slot() else {
            return false;
        };
        sqe.opcode = IORING_OP_ACCEPT;
        sqe.fd = fd;
        sqe.addr = addr as u64;
        sqe.off = addrlen as u64;
        sqe.rw_flags = flags;
        sqe.user_data = user_data;
        true
    }

    /// Build and push a `Connect` SQE.
    ///
    /// # Safety
    ///
    /// `addr..addr + addrlen` must be a valid socket address that stays
    /// valid and unmoved until the CQE has been consumed.
    pub unsafe fn push_connect(
        &mut self,
        user_data: u64,
        fd: i32,
        addr: *const libc::sockaddr,
        addrlen: libc::socklen_t,
    ) -> bool {
        let Some(sqe) = self.slot() else {
            return false;
        };
        sqe.opcode = IORING_OP_CONNECT;
        sqe.fd = fd;
        sqe.addr = addr as u64;
        sqe.off = addrlen as u64;
        sqe.user_data = user_data;
        true
    }

    /// Build and push a `Timeout` SQE.
    ///
    /// Completes with `-ETIME` when `ts` elapses, or with `0` once `count`
    /// other completions have been posted (`count = 0`: time only).
    ///
    /// # Safety
    ///
    /// `ts` must point to a valid `libc::timespec` (on 64-bit Linux it has
    /// the `__kernel_timespec` layout) that stays alive and unmodified until
    /// the SQE has been submitted. The kernel copies the timespec while it
    /// prepares the request (`io_timeout_prep`), i.e. inside the
    /// `io_uring_enter` that submits it, or in the SQ thread with SQPOLL;
    /// it does not read it again before the CQE. Keeping it alive until the
    /// CQE is the conservative choice.
    pub unsafe fn push_timeout(
        &mut self,
        user_data: u64,
        ts: *const libc::timespec,
        count: u32,
        flags: u32,
    ) -> bool {
        let Some(sqe) = self.slot() else {
            return false;
        };
        sqe.opcode = IORING_OP_TIMEOUT;
        sqe.fd = -1;
        sqe.addr = ts as u64;
        // The kernel requires len == 1 (one timespec) and takes the
        // completion count from `off`; anything else is -EINVAL.
        sqe.len = 1;
        sqe.off = u64::from(count);
        sqe.rw_flags = flags;
        sqe.user_data = user_data;
        true
    }

    /// Build and push a `LinkTimeout` SQE (linked to the previous SQE).
    ///
    /// The previous SQE must have been pushed with `IOSQE_IO_LINK` flag.
    ///
    /// # Safety
    ///
    /// Same as [`push_timeout`](Self::push_timeout).
    pub unsafe fn push_link_timeout(&mut self, user_data: u64, ts: *const libc::timespec) -> bool {
        let Some(sqe) = self.slot() else {
            return false;
        };
        sqe.opcode = IORING_OP_LINK_TIMEOUT;
        sqe.fd = -1;
        sqe.addr = ts as u64;
        sqe.len = 1;
        sqe.user_data = user_data;
        true
    }

    /// Build and push an `AsyncCancel` SQE.
    ///
    /// Cancels a pending operation identified by `target_user_data`. Takes
    /// no pointers: `addr` carries the target's user_data, not an address.
    pub fn push_cancel(&mut self, user_data: u64, target_user_data: u64) -> bool {
        let Some(sqe) = self.slot() else {
            return false;
        };
        sqe.opcode = IORING_OP_ASYNC_CANCEL;
        sqe.fd = -1;
        sqe.addr = target_user_data;
        sqe.user_data = user_data;
        true
    }

    /// Build and push a `Close` SQE.
    ///
    /// # Safety
    ///
    /// The caller must own `fd` and give up ownership: nothing may use or
    /// close it after this call (closing an fd owned by someone else, e.g.
    /// an `OwnedFd`, breaks I/O safety, and its number may be reused).
    pub unsafe fn push_close(&mut self, user_data: u64, fd: i32) -> bool {
        let Some(sqe) = self.slot() else {
            return false;
        };
        sqe.opcode = IORING_OP_CLOSE;
        sqe.fd = fd;
        sqe.user_data = user_data;
        true
    }

    /// Set the `IOSQE_IO_LINK` flag on the most recently pushed SQE.
    ///
    /// Links the previous SQE to the next one. If the previous SQE fails
    /// or is cancelled, the linked SQE is also cancelled.
    ///
    /// Returns `false` if no SQE has been pushed yet.
    pub fn link_last(&mut self) -> bool {
        if self.sq_tail == self.load_head() {
            return false;
        }
        let prev_idx = self.sq_tail.wrapping_sub(1) & self.ring_mask;
        let sqe = unsafe { &mut *self.sqes.add(prev_idx as usize) };
        sqe.flags |= IOSQE_IO_LINK;
        true
    }
}
