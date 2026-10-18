//! Accept operation (`IORING_OP_ACCEPT`).

use std::net::SocketAddr;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use super::addr::RawAddr;
use super::{Accept, Op, Operation, sealed};
use crate::driver::Driver;
use crate::error::MeguriResult;
use crate::sq::SubmissionQueue;

/// Accepts one connection. Output: the new socket (close-on-exec) and the
/// peer address. IPv4 and IPv6 listeners only.
pub struct AcceptOp {
    fd: RawFd,
    addr: Box<RawAddr>,
}

impl Accept {
    /// Accept a connection on listening socket `fd`.
    pub fn new(driver: &Driver, fd: RawFd) -> Self {
        Op::submit_later(
            driver,
            AcceptOp {
                fd,
                addr: RawAddr::empty(),
            },
        )
    }
}

impl sealed::Sealed for AcceptOp {}

impl Operation for AcceptOp {
    type Output = MeguriResult<(OwnedFd, SocketAddr)>;

    fn push(&mut self, sq: &mut SubmissionQueue, user_data: u64) -> bool {
        sq.push_accept(
            user_data,
            self.fd,
            (&raw mut self.addr.storage).cast(),
            &raw mut self.addr.len,
            libc::SOCK_CLOEXEC as u32,
        )
    }

    fn complete(self, result: MeguriResult<u32>) -> Self::Output {
        // SAFETY: a successful accept returns a new fd that nothing else owns.
        let socket = unsafe { OwnedFd::from_raw_fd(result? as RawFd) };
        Ok((socket, self.addr.to_std()?))
    }

    /// The future was dropped, but a connection was accepted anyway: close
    /// it so the fd does not leak.
    fn abandon(self, res: i32) {
        if res >= 0 {
            // SAFETY: as in `complete`; dropping closes it.
            drop(unsafe { OwnedFd::from_raw_fd(res) });
        }
    }
}
