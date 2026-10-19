//! Connect operation (`IORING_OP_CONNECT`).

use std::net::SocketAddr;
use std::os::fd::RawFd;

use super::addr::RawAddr;
use super::{Connect, Op, Operation, sealed};
use crate::driver::Driver;
use crate::error::MeguriResult;
use crate::sq::SubmissionQueue;

/// Connects a socket to a peer.
pub struct ConnectOp {
    fd: RawFd,
    addr: Box<RawAddr>,
}

impl Connect {
    /// Connect socket `fd` (matching `addr`'s family) to `addr`.
    pub fn new(driver: &Driver, fd: RawFd, addr: SocketAddr) -> Self {
        Op::submit_later(
            driver,
            ConnectOp {
                fd,
                addr: RawAddr::from_std(addr),
            },
        )
    }
}

impl sealed::Sealed for ConnectOp {}

impl Operation for ConnectOp {
    type Output = MeguriResult<()>;

    fn push(&mut self, sq: &mut SubmissionQueue, user_data: u64) -> bool {
        // SAFETY: `addr` is boxed (stable address) and holds an encoded
        // sockaddr of `addr.len` bytes; it lives until the CQE is consumed,
        // as described for `AcceptOp::push`.
        unsafe {
            sq.push_connect(
                user_data,
                self.fd,
                (&raw const self.addr.storage).cast(),
                self.addr.len,
            )
        }
    }

    fn complete(self, result: MeguriResult<u32>) -> Self::Output {
        result.map(|_| ())
    }
}
