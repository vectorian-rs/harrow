//! Submission queue management.
//!
//! Wraps the kernel-mapped SQ ring and SQE array. SQEs are built here and
//! submitted to the kernel via `Ring::submit()`.

use std::sync::atomic::{AtomicU32, Ordering};

use crate::syscall::{
    IORING_OP_ACCEPT, IORING_OP_ASYNC_CANCEL, IORING_OP_CLOSE, IORING_OP_LINK_TIMEOUT,
    IORING_OP_NOP, IORING_OP_READ, IORING_OP_RECV, IORING_OP_SEND, IORING_OP_TIMEOUT,
    IORING_OP_WRITE, IOSQE_IO_LINK, IoUringParams, IoUringSqe,
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
    array: *mut u32,

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
            // This means ring position i always maps to SQE index i.
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
                array,
                sqes,
                sq_tail,
            }
        }
    }

    /// Get a mutable reference to the next available SQE slot.
    ///
    /// Returns `None` if the SQ is full (local tail has caught up with
    /// the kernel head by `ring_entries`).
    pub fn next_sqe(&mut self) -> Option<&mut IoUringSqe> {
        let head = self.load_head();
        if self.sq_tail.wrapping_sub(head) >= self.ring_entries {
            return None; // SQ is full
        }
        let idx = self.sq_tail & self.ring_mask;
        let sqe = unsafe { &mut *self.sqes.add(idx as usize) };
        // Zero the SQE so callers start from a clean slate.
        *sqe = IoUringSqe::zeroed();
        self.sq_tail = self.sq_tail.wrapping_add(1);
        Some(sqe)
    }

    /// Push a pre-built SQE into the next available slot.
    /// Returns `true` if pushed, `false` if the SQ is full.
    pub fn push(&mut self, sqe: &IoUringSqe) -> bool {
        let head = self.load_head();
        if self.sq_tail.wrapping_sub(head) >= self.ring_entries {
            return false;
        }
        let idx = self.sq_tail & self.ring_mask;
        unsafe { std::ptr::copy_nonoverlapping(sqe, self.sqes.add(idx as usize), 1) };
        self.sq_tail = self.sq_tail.wrapping_add(1);
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
        let Some(sqe) = self.next_sqe() else {
            return false;
        };
        sqe.opcode = IORING_OP_NOP;
        sqe.fd = -1;
        sqe.user_data = user_data;
        true
    }

    /// Build and push a `Read` SQE.
    pub fn push_read(
        &mut self,
        user_data: u64,
        fd: i32,
        buf: *mut u8,
        len: u32,
        offset: u64,
    ) -> bool {
        let Some(sqe) = self.next_sqe() else {
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
    pub fn push_write(
        &mut self,
        user_data: u64,
        fd: i32,
        buf: *const u8,
        len: u32,
        offset: u64,
    ) -> bool {
        let Some(sqe) = self.next_sqe() else {
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
    pub fn push_send(
        &mut self,
        user_data: u64,
        fd: i32,
        buf: *const u8,
        len: u32,
        flags: u32,
    ) -> bool {
        let Some(sqe) = self.next_sqe() else {
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
    pub fn push_recv(
        &mut self,
        user_data: u64,
        fd: i32,
        buf: *mut u8,
        len: u32,
        flags: u32,
    ) -> bool {
        let Some(sqe) = self.next_sqe() else {
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
    pub fn push_accept(
        &mut self,
        user_data: u64,
        fd: i32,
        addr: *mut libc::sockaddr,
        addrlen: *mut libc::socklen_t,
        flags: u32,
    ) -> bool {
        let Some(sqe) = self.next_sqe() else {
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

    /// Build and push a `Timeout` SQE.
    ///
    /// `ts` must remain valid until the CQE is reaped.
    pub fn push_timeout(
        &mut self,
        user_data: u64,
        ts: *const libc::timespec,
        count: u32,
        flags: u32,
    ) -> bool {
        let Some(sqe) = self.next_sqe() else {
            return false;
        };
        sqe.opcode = IORING_OP_TIMEOUT;
        sqe.fd = -1;
        sqe.addr = ts as u64;
        sqe.len = count;
        sqe.rw_flags = flags;
        sqe.user_data = user_data;
        true
    }

    /// Build and push a `LinkTimeout` SQE (linked to the previous SQE).
    ///
    /// The previous SQE must have been pushed with `IOSQE_IO_LINK` flag.
    /// `ts` must remain valid until the CQE is reaped.
    pub fn push_link_timeout(
        &mut self,
        user_data: u64,
        ts: *const libc::timespec,
    ) -> bool {
        let Some(sqe) = self.next_sqe() else {
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
    /// Cancels a pending operation identified by `target_user_data`.
    pub fn push_cancel(&mut self, user_data: u64, target_user_data: u64) -> bool {
        let Some(sqe) = self.next_sqe() else {
            return false;
        };
        sqe.opcode = IORING_OP_ASYNC_CANCEL;
        sqe.fd = -1;
        sqe.addr = target_user_data;
        sqe.user_data = user_data;
        true
    }

    /// Build and push a `Close` SQE.
    pub fn push_close(&mut self, user_data: u64, fd: i32) -> bool {
        let Some(sqe) = self.next_sqe() else {
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
