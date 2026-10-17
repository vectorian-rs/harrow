//! Send operation (`IORING_OP_SEND`).

use std::os::fd::RawFd;

use bytes::Bytes;

use super::{Op, Operation, Send, sealed, sqe_len};
use crate::driver::Driver;
use crate::error::MeguriResult;
use crate::sq::SubmissionQueue;

/// Sends the buffer. Output: `(bytes sent, buffer)`; a short send is
/// possible, so check the count.
pub struct SendOp {
    fd: RawFd,
    buf: Bytes,
    flags: u32,
}

impl Send {
    /// Send on socket `fd` with `send(2)` `flags`. Pass `MSG_NOSIGNAL` to
    /// get `EPIPE` instead of `SIGPIPE` when the peer is gone.
    pub fn new(driver: &Driver, fd: RawFd, buf: Bytes, flags: u32) -> Self {
        Op::submit_later(driver, SendOp { fd, buf, flags })
    }
}

impl sealed::Sealed for SendOp {}

impl Operation for SendOp {
    type Output = (MeguriResult<usize>, Bytes);

    fn push(&mut self, sq: &mut SubmissionQueue, user_data: u64) -> bool {
        sq.push_send(
            user_data,
            self.fd,
            self.buf.as_ptr(),
            sqe_len(self.buf.len()),
            self.flags,
        )
    }

    fn complete(self, result: MeguriResult<u32>) -> Self::Output {
        (result.map(|n| n as usize), self.buf)
    }
}
