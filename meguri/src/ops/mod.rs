//! Owned-buffer I/O operations as futures.
//!
//! Every operation is an [`Op`]: a future that pushes its SQE on first poll
//! and resolves when the kernel posts the CQE. Operations take ownership of
//! their buffers and give them back in the output, so the kernel never
//! writes into memory the caller can still touch.
//!
//! Dropping an `Op` before it completes is safe: its buffers move to the
//! ring's reclaim pool until the kernel is done with them, and an
//! `ASYNC_CANCEL` is queued so an idle operation does not hold them forever.
//!
//! ```ignore
//! use bytes::BytesMut;
//! use meguri::{Driver, ops::Read};
//!
//! let driver = Driver::new(64)?;
//! let (res, buf) = driver.block_on(Read::new(&driver, fd, BytesMut::with_capacity(4096), 0));
//! let n = res?;
//! ```
//!
//! File descriptors are borrowed as `RawFd`: keep them open until the
//! operation completes. A closed fd makes the kernel return `EBADF`; it is
//! not a memory-safety issue.

use std::future::Future;
use std::io;
use std::mem;
use std::pin::Pin;
use std::task::{Context, Poll};

use crate::driver::Driver;
use crate::error::{MeguriError, MeguriResult};
use crate::reclaim::Reclaimable;
use crate::sq::SubmissionQueue;

pub mod accept;
pub mod cancel;
pub mod connect;
pub mod read;
pub mod recv;
pub mod send;
pub mod timeout;
pub mod write;

pub use accept::Accept;
pub use cancel::Cancel;
pub use connect::Connect;
pub use read::ReadOp;
pub use recv::Recv;
pub use send::Send;
pub use timeout::Timeout;
pub use write::WriteOp;

/// Read into a buffer's spare capacity; see [`ReadOp`].
pub type Read = Op<ReadOp>;
/// Write a buffer; see [`WriteOp`].
pub type Write = Op<WriteOp>;

mod sealed {
    pub trait Sealed {}
}

/// What one kind of operation contributes to [`Op`]: its SQE and how to turn
/// the kernel's result into an output. Implemented only inside meguri.
pub trait Operation: sealed::Sealed + std::marker::Send + 'static {
    type Output;

    /// Push this operation's SQE. Returns `false` if the SQ is full.
    ///
    /// Any memory the SQE points to must be owned by `self` and must not
    /// move when `self` moves (heap buffers, boxed structs).
    fn push(&mut self, sq: &mut SubmissionQueue, user_data: u64) -> bool;

    /// Build the output from the CQE result (`res` when non-negative).
    fn complete(self, result: MeguriResult<u32>) -> Self::Output;

    /// The owning future was dropped and the kernel has now finished with
    /// this operation; `res` is the raw CQE result. Default: drop resources.
    fn abandon(self, res: i32)
    where
        Self: Sized,
    {
        let _ = res;
    }
}

/// A submitted-on-first-poll io_uring operation. See the [module docs](self).
#[must_use = "operations do nothing unless awaited"]
pub struct Op<K: Operation> {
    driver: Driver,
    state: State<K>,
}

enum State<K> {
    Unsubmitted(K),
    InFlight { user_data: u64, kind: K },
    Done,
}

impl<K: Operation> Op<K> {
    fn submit_later(driver: &Driver, kind: K) -> Self {
        Self {
            driver: driver.clone(),
            state: State::Unsubmitted(kind),
        }
    }

    /// The `user_data` of the submitted SQE, once the operation has been
    /// polled and until it completes. Used with [`Cancel`](cancel::Cancel).
    pub fn user_data(&self) -> Option<u64> {
        match self.state {
            State::InFlight { user_data, .. } => Some(user_data),
            _ => None,
        }
    }
}

impl<K: Operation> Unpin for Op<K> {}

impl<K: Operation> Future for Op<K> {
    type Output = K::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<K::Output> {
        let this = self.get_mut();
        let mut ring = this.driver.ring();

        match mem::replace(&mut this.state, State::Done) {
            State::Unsubmitted(mut kind) => {
                let user_data = ring.dispatcher().register(Some(cx.waker().clone()));
                let pushed = kind.push(ring.sq(), user_data)
                    // SQ full: hand what is queued to the kernel, retry once.
                    || (ring.submit().is_ok() && kind.push(ring.sq(), user_data));
                if !pushed {
                    ring.dispatcher().unregister(user_data);
                    return Poll::Ready(kind.complete(Err(MeguriError::SqFull)));
                }
                this.state = State::InFlight { user_data, kind };
                Poll::Pending
            }
            State::InFlight { user_data, kind } => {
                match ring.dispatcher().take_completion(user_data) {
                    Some((res, _flags)) => Poll::Ready(kind.complete(result_from_res(res))),
                    None => {
                        ring.dispatcher().update_waker(user_data, cx.waker());
                        this.state = State::InFlight { user_data, kind };
                        Poll::Pending
                    }
                }
            }
            State::Done => panic!("meguri Op polled after completion"),
        }
    }
}

impl<K: Operation> Drop for Op<K> {
    fn drop(&mut self) {
        let State::InFlight { user_data, kind } = mem::replace(&mut self.state, State::Done) else {
            return;
        };
        match self.driver.try_ring() {
            Some(mut ring) => ring.release_op(user_data, Abandoned(kind)),
            // Dropped while the ring is borrowed (e.g. from a waker during
            // `Driver::turn`). The kernel may still use the buffers, so they
            // must not be freed: leak them rather than risk use-after-free.
            None => mem::forget(kind),
        }
    }
}

/// An operation's resources after its future was dropped.
struct Abandoned<K>(K);

impl<K: Operation> Reclaimable for Abandoned<K> {
    fn reclaim(self: Box<Self>, result: i32) {
        self.0.abandon(result);
    }
}

/// Map a CQE `res` to a result: non-negative is success, negative is `-errno`.
fn result_from_res(res: i32) -> MeguriResult<u32> {
    if res >= 0 {
        Ok(res as u32)
    } else if res == -libc::ECANCELED {
        Err(MeguriError::Cancelled)
    } else {
        Err(MeguriError::Io(io::Error::from_raw_os_error(-res)))
    }
}

/// Clamp a buffer length to what one SQE can carry.
fn sqe_len(len: usize) -> u32 {
    u32::try_from(len).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use std::os::fd::RawFd;

    use bytes::{Bytes, BytesMut};

    use super::*;

    fn pipe() -> (RawFd, RawFd) {
        let mut fds = [0; 2];
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        (fds[0], fds[1])
    }

    fn close(fd: RawFd) {
        unsafe { libc::close(fd) };
    }

    /// Polls `op` once, then drops it.
    async fn poll_once_then_drop<K: Operation>(mut op: Op<K>) {
        std::future::poll_fn(|cx| {
            assert!(Pin::new(&mut op).poll(cx).is_pending());
            Poll::Ready(())
        })
        .await;
    }

    #[test]
    fn write_then_read_through_a_pipe() {
        let driver = Driver::new(8).expect("ring");
        let (r, w) = pipe();

        let (res, buf) = driver.block_on(async {
            let (written, _) = Write::new(&driver, w, Bytes::from_static(b"hello"), 0).await;
            assert_eq!(written.expect("write"), 5);
            let mut buf = BytesMut::with_capacity(64);
            buf.extend_from_slice(b">");
            Read::new(&driver, r, buf, 0).await
        });

        assert_eq!(res.expect("read"), 5);
        assert_eq!(&buf[..], b">hello", "read appends after existing bytes");
        assert!(driver.ring().dispatcher().is_empty());
        close(r);
        close(w);
    }

    #[test]
    fn errors_return_the_buffer() {
        let driver = Driver::new(8).expect("ring");
        let mut buf = BytesMut::with_capacity(16);
        buf.extend_from_slice(b"keep");

        let (res, buf) = driver.block_on(Read::new(&driver, -1, buf, 0));

        let Err(MeguriError::Io(err)) = res else {
            panic!("expected EBADF, got {res:?}");
        };
        assert_eq!(err.raw_os_error(), Some(libc::EBADF));
        assert_eq!(&buf[..], b"keep");
    }

    #[test]
    fn dropping_an_in_flight_read_cancels_it_and_reclaims_the_buffer() {
        let driver = Driver::new(8).expect("ring");
        let (r, w) = pipe();

        // Nothing is written, so the read stays in flight until cancelled.
        let read = Read::new(&driver, r, BytesMut::with_capacity(64), 0);
        driver.block_on(poll_once_then_drop(read));
        assert_eq!(
            driver.ring().reclaim().len(),
            1,
            "buffer held for the kernel"
        );

        while driver.ring().dispatcher().in_flight() > 0 {
            driver.turn(true).expect("turn");
        }
        assert!(
            driver.ring().reclaim().is_empty(),
            "buffer released after cancel"
        );
        assert!(driver.ring().dispatcher().is_empty());
        close(r);
        close(w);
    }
}
