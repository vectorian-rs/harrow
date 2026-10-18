//! Driver: a shared handle to a [`Ring`] that runs [`ops`](crate::ops).
//!
//! Operations need the ring three times: to push their SQE, to park their
//! waker, and to hand their buffers to the reclaim pool if they are dropped
//! early. A `Driver` is a cheap, clonable handle that gives them that access.
//! It is passed explicitly (no thread-local) and is single-threaded, like the
//! ring itself (`IORING_SETUP_SINGLE_ISSUER`).
//!
//! Something has to submit SQEs and process CQEs. [`Driver::block_on`] does
//! that for one future; an existing event loop calls [`Driver::turn`] instead.

use std::cell::{RefCell, RefMut};
use std::future::Future;
use std::io;
use std::pin::pin;
use std::rc::Rc;
use std::task::{Context, Poll, Waker};

use crate::ring::Ring;

/// A shared, single-threaded handle to a [`Ring`].
#[derive(Clone)]
pub struct Driver {
    ring: Rc<RefCell<Ring>>,
}

impl Driver {
    /// Create a ring with `entries` SQ slots and wrap it.
    pub fn new(entries: u32) -> io::Result<Self> {
        Ok(Self::from_ring(Ring::new(entries)?))
    }

    /// Wrap an existing ring.
    pub fn from_ring(ring: Ring) -> Self {
        Self {
            ring: Rc::new(RefCell::new(ring)),
        }
    }

    /// Submit queued SQEs and process available completions, waking the
    /// futures they belong to. With `wait`, block until at least one
    /// completion arrives, unless nothing is in flight. Returns the number of
    /// completions processed.
    pub fn turn(&self, wait: bool) -> io::Result<usize> {
        let mut ring = self.ring.borrow_mut();
        let min_complete = u32::from(wait && ring.dispatcher().in_flight() > 0);
        loop {
            match ring.submit_and_wait(min_complete) {
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) => return Err(err),
                Ok(_) => return Ok(ring.poll_completions()),
            }
        }
    }

    /// Run `future` to completion on this thread, driving the ring.
    ///
    /// # Panics
    ///
    /// If the future is pending while no meguri operation is in flight (it
    /// is waiting on something this driver cannot complete), or if
    /// `io_uring_enter` fails.
    pub fn block_on<F: Future>(&self, future: F) -> F::Output {
        let mut future = pin!(future);
        // Every completion batch re-polls the future, so wakers need not
        // do anything.
        let mut cx = Context::from_waker(Waker::noop());
        loop {
            if let Poll::Ready(output) = future.as_mut().poll(&mut cx) {
                return output;
            }
            assert!(
                self.ring.borrow_mut().dispatcher().in_flight() > 0,
                "Driver::block_on: future is pending but no meguri operation is in flight"
            );
            self.turn(true).expect("io_uring_enter failed");
        }
    }

    /// Borrow the ring. Panics if it is already borrowed, e.g. when called
    /// from a waker that runs during [`Driver::turn`].
    pub(crate) fn ring(&self) -> RefMut<'_, Ring> {
        self.ring.borrow_mut()
    }

    /// Borrow the ring unless it is already borrowed.
    pub(crate) fn try_ring(&self) -> Option<RefMut<'_, Ring>> {
        self.ring.try_borrow_mut().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_on_returns_a_ready_future() {
        let driver = Driver::new(8).expect("ring");
        assert_eq!(driver.block_on(async { 42 }), 42);
    }

    #[test]
    #[should_panic(expected = "no meguri operation is in flight")]
    fn block_on_panics_instead_of_hanging() {
        let driver = Driver::new(8).expect("ring");
        driver.block_on(std::future::pending::<()>());
    }

    #[test]
    fn turn_without_work_returns_immediately() {
        let driver = Driver::new(8).expect("ring");
        assert_eq!(driver.turn(true).expect("turn"), 0);
    }
}
