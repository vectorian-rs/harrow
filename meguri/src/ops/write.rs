//! Write operation (`IORING_OP_WRITE`).

use std::os::fd::RawFd;

use bytes::Bytes;

use super::{Op, Operation, Write, sealed, sqe_len};
use crate::driver::Driver;
use crate::error::MeguriResult;
use crate::sq::SubmissionQueue;

/// Writes the buffer. Output: `(bytes written, buffer)`; a short write is
/// possible, so check the count.
pub struct WriteOp {
    fd: RawFd,
    buf: Bytes,
    offset: u64,
}

impl Write {
    /// Write to `fd` at `offset`. Use `u64::MAX` for the current file
    /// position; pipes and sockets need `0` or `u64::MAX`.
    pub fn new(driver: &Driver, fd: RawFd, buf: Bytes, offset: u64) -> Self {
        Op::submit_later(driver, WriteOp { fd, buf, offset })
    }
}

impl sealed::Sealed for WriteOp {}

impl Operation for WriteOp {
    type Output = (MeguriResult<usize>, Bytes);

    fn push(&mut self, sq: &mut SubmissionQueue, user_data: u64) -> bool {
        sq.push_write(
            user_data,
            self.fd,
            self.buf.as_ptr(),
            sqe_len(self.buf.len()),
            self.offset,
        )
    }

    fn complete(self, result: MeguriResult<u32>) -> Self::Output {
        (result.map(|n| n as usize), self.buf)
    }
}
