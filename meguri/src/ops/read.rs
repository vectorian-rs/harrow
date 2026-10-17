//! Read operation (`IORING_OP_READ`).

use std::os::fd::RawFd;

use bytes::BytesMut;

use super::{Op, Operation, Read, sealed, sqe_len};
use crate::driver::Driver;
use crate::error::MeguriResult;
use crate::sq::SubmissionQueue;

/// Reads into the buffer's spare capacity (`capacity - len`) and appends
/// what was read. Output: `(bytes read, buffer)`; the buffer comes back on
/// error too. Reserve capacity first: a full buffer reads 0 bytes.
pub struct ReadOp {
    fd: RawFd,
    buf: BytesMut,
    offset: u64,
}

impl Read {
    /// Read from `fd` at `offset`. Use `u64::MAX` for the current file
    /// position; pipes and sockets need `0` or `u64::MAX`.
    pub fn new(driver: &Driver, fd: RawFd, buf: BytesMut, offset: u64) -> Self {
        Op::submit_later(driver, ReadOp { fd, buf, offset })
    }
}

impl sealed::Sealed for ReadOp {}

impl Operation for ReadOp {
    type Output = (MeguriResult<usize>, BytesMut);

    fn push(&mut self, sq: &mut SubmissionQueue, user_data: u64) -> bool {
        let spare = self.buf.spare_capacity_mut();
        sq.push_read(
            user_data,
            self.fd,
            spare.as_mut_ptr().cast(),
            sqe_len(spare.len()),
            self.offset,
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
