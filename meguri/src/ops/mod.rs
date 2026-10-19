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
mod addr;
pub mod cancel;
pub mod connect;
pub mod read;
pub mod recv;
pub mod send;
pub mod timeout;
pub mod write;

pub use accept::AcceptOp;
pub use cancel::{CancelOp, CancelResult};
pub use connect::ConnectOp;
pub use read::ReadOp;
pub use recv::RecvOp;
pub use send::SendOp;
pub use timeout::TimeoutOp;
pub use write::WriteOp;

/// Accept a connection; see [`AcceptOp`].
pub type Accept = Op<AcceptOp>;
/// Cancel another operation; see [`CancelOp`].
pub type Cancel = Op<CancelOp>;
/// Connect a socket; see [`ConnectOp`].
pub type Connect = Op<ConnectOp>;
/// Read into a buffer's spare capacity; see [`ReadOp`].
pub type Read = Op<ReadOp>;
/// Receive into a buffer's spare capacity; see [`RecvOp`].
pub type Recv = Op<RecvOp>;
/// Send a buffer; see [`SendOp`].
pub type Send = Op<SendOp>;
/// Kernel timer; see [`TimeoutOp`].
pub type Timeout = Op<TimeoutOp>;
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

    /// Before the ring is dropped (and with it the reclaim pool), the kernel
    /// must have finished with every abandoned buffer. Here the read is
    /// dropped without the driver ever submitting it or its cancel.
    #[test]
    fn ring_drain_waits_for_an_abandoned_read_before_releasing_its_buffer() {
        let driver = Driver::new(8).expect("ring");
        let (r, w) = pipe();

        let read = Read::new(&driver, r, BytesMut::with_capacity(64), 0);
        driver.block_on(poll_once_then_drop(read));
        assert_eq!(driver.ring().reclaim().len(), 1);

        assert!(
            driver
                .ring()
                .drain_in_flight(std::time::Duration::from_secs(5))
        );
        assert!(
            driver.ring().reclaim().is_empty(),
            "kernel finished; buffer released"
        );
        // Only meguri's own timer and its cancel may remain.
        assert!(
            driver
                .ring()
                .dispatcher()
                .cancellable_user_data()
                .is_empty()
        );
        close(r);
        close(w);
    }

    /// Dropping the last driver handle with an abandoned operation still in
    /// the kernel cancels and waits for it instead of freeing its buffer
    /// early, and it does not hang.
    #[test]
    fn dropping_the_driver_with_an_abandoned_read_returns_promptly() {
        let driver = Driver::new(8).expect("ring");
        let (r, w) = pipe();

        let read = Read::new(&driver, r, BytesMut::with_capacity(64), 0);
        driver.block_on(poll_once_then_drop(read));
        let started = std::time::Instant::now();
        drop(driver);
        assert!(started.elapsed() < std::time::Duration::from_millis(900));
        close(r);
        close(w);
    }

    /// Polls two futures until both are ready.
    async fn join<A: Future, B: Future>(a: A, b: B) -> (A::Output, B::Output) {
        let (mut a, mut b) = (Box::pin(a), Box::pin(b));
        let (mut out_a, mut out_b) = (None, None);
        std::future::poll_fn(|cx| {
            if out_a.is_none()
                && let Poll::Ready(v) = a.as_mut().poll(cx)
            {
                out_a = Some(v);
            }
            if out_b.is_none()
                && let Poll::Ready(v) = b.as_mut().poll(cx)
            {
                out_b = Some(v);
            }
            if out_a.is_some() && out_b.is_some() {
                Poll::Ready((out_a.take().unwrap(), out_b.take().unwrap()))
            } else {
                Poll::Pending
            }
        })
        .await
    }

    fn tcp_socket() -> std::os::fd::OwnedFd {
        use std::os::fd::FromRawFd;
        let fd = unsafe { libc::socket(libc::AF_INET, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0) };
        assert!(fd >= 0);
        unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) }
    }

    #[test]
    fn accept_connect_send_recv_over_loopback() {
        use std::os::fd::AsRawFd;

        let driver = Driver::new(16).expect("ring");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = tcp_socket();

        driver.block_on(async {
            let (accepted, connected) = join(
                Accept::new(&driver, listener.as_raw_fd()),
                Connect::new(&driver, client.as_raw_fd(), addr),
            )
            .await;
            connected.expect("connect");
            let (server, peer) = accepted.expect("accept");
            assert!(peer.ip().is_loopback());

            let (sent, _) = Send::new(
                &driver,
                client.as_raw_fd(),
                Bytes::from_static(b"ping"),
                libc::MSG_NOSIGNAL as u32,
            )
            .await;
            assert_eq!(sent.expect("send"), 4);
            let (got, buf) =
                Recv::new(&driver, server.as_raw_fd(), BytesMut::with_capacity(16), 0).await;
            assert_eq!(got.expect("recv"), 4);
            assert_eq!(&buf[..], b"ping");

            drop(server);
            let (eof, _) =
                Recv::new(&driver, client.as_raw_fd(), BytesMut::with_capacity(16), 0).await;
            assert_eq!(eof.expect("recv after close"), 0, "peer closed");
        });
        assert!(driver.ring().dispatcher().is_empty());
    }

    #[test]
    fn connect_to_a_closed_port_fails() {
        use std::os::fd::AsRawFd;

        let driver = Driver::new(8).expect("ring");
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let client = tcp_socket();

        let Err(MeguriError::Io(err)) =
            driver.block_on(Connect::new(&driver, client.as_raw_fd(), addr))
        else {
            panic!("connect should be refused");
        };
        assert_eq!(err.raw_os_error(), Some(libc::ECONNREFUSED));
    }

    #[test]
    fn dropping_an_in_flight_accept_cancels_it() {
        use std::os::fd::AsRawFd;

        let driver = Driver::new(8).expect("ring");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();

        driver.block_on(poll_once_then_drop(Accept::new(
            &driver,
            listener.as_raw_fd(),
        )));
        assert_eq!(driver.ring().reclaim().len(), 1);
        while driver.ring().dispatcher().in_flight() > 0 {
            driver.turn(true).expect("turn");
        }
        assert!(driver.ring().reclaim().is_empty());
        assert!(driver.ring().dispatcher().is_empty());
    }

    #[test]
    fn timeout_completes_after_its_duration() {
        let driver = Driver::new(8).expect("ring");
        let started = std::time::Instant::now();
        driver
            .block_on(Timeout::new(&driver, std::time::Duration::from_millis(20)))
            .expect("timeout");
        assert!(started.elapsed() >= std::time::Duration::from_millis(20));
    }

    #[test]
    fn dropping_a_long_timeout_cancels_it_promptly() {
        let driver = Driver::new(8).expect("ring");
        let started = std::time::Instant::now();

        driver.block_on(poll_once_then_drop(Timeout::new(
            &driver,
            std::time::Duration::from_secs(30),
        )));
        while driver.ring().dispatcher().in_flight() > 0 {
            driver.turn(true).expect("turn");
        }
        assert!(started.elapsed() < std::time::Duration::from_secs(5));
        assert!(driver.ring().reclaim().is_empty());
    }

    #[test]
    fn cancel_stops_an_operation_that_is_still_awaited() {
        let driver = Driver::new(8).expect("ring");
        let (r, w) = pipe();

        driver.block_on(async {
            let mut read = Read::new(&driver, r, BytesMut::with_capacity(16), 0);
            std::future::poll_fn(|cx| {
                assert!(Pin::new(&mut read).poll(cx).is_pending());
                Poll::Ready(())
            })
            .await;
            let target = read.user_data().expect("submitted");

            let outcome = Cancel::new(&driver, target).await.expect("cancel");
            assert_eq!(outcome, CancelResult::Cancelled);
            let (res, _) = read.await;
            assert!(matches!(res, Err(MeguriError::Cancelled)), "{res:?}");
        });

        let missing = driver
            .block_on(Cancel::new(&driver, 9_999))
            .expect("cancel");
        assert_eq!(missing, CancelResult::NotFound);
        close(r);
        close(w);
    }
}
