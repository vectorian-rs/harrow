//! Recv operation (`IORING_OP_RECV`).

use std::os::fd::RawFd;

use bytes::BytesMut;

use super::{Op, Operation, Recv, sealed, sqe_len};
use crate::driver::Driver;
use crate::error::MeguriResult;
use crate::sq::SubmissionQueue;

/// Receives into the buffer's spare capacity and appends what arrived.
/// Output: `(bytes received, buffer)`; `0` means the peer closed. The
/// buffer comes back on error too.
pub struct RecvOp {
    fd: RawFd,
    buf: BytesMut,
    flags: u32,
}

impl Recv {
    /// Receive from socket `fd` with `recv(2)` `flags`.
    pub fn new(driver: &Driver, fd: RawFd, buf: BytesMut, flags: u32) -> Self {
        Op::submit_later(driver, RecvOp { fd, buf, flags })
    }
}

impl sealed::Sealed for RecvOp {}

impl Operation for RecvOp {
    type Output = (MeguriResult<usize>, BytesMut);

    fn push(&mut self, sq: &mut SubmissionQueue, user_data: u64) -> bool {
        let spare = self.buf.spare_capacity_mut();
        sq.push_recv(
            user_data,
            self.fd,
            spare.as_mut_ptr().cast(),
            sqe_len(spare.len()),
            self.flags,
        )
    }

    fn complete(mut self, result: MeguriResult<u32>) -> Self::Output {
        let result = result.map(|n| {
            let n = n as usize;
            // SAFETY: the kernel initialized `n` bytes of spare capacity,
            // and `n` is at most the length we passed in `push`.
            unsafe { self.buf.set_len(self.buf.len() + n) };
            n
        });
        (result, self.buf)
    }
}
