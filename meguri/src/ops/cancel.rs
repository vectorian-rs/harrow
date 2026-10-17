//! Async cancel operation (`IORING_OP_ASYNC_CANCEL`).
//!
//! Dropping an [`Op`](super::Op) already cancels it. Use `Cancel` to stop an
//! operation you keep awaiting, e.g. to end a read early: the cancelled
//! operation then resolves with [`MeguriError::Cancelled`].

use super::{Cancel, Op, Operation, sealed};
use crate::driver::Driver;
use crate::error::{MeguriError, MeguriResult};
use crate::sq::SubmissionQueue;

/// What happened to the target operation.
#[derive(Debug, PartialEq, Eq)]
pub enum CancelResult {
    /// The target was cancelled; it completes with `Cancelled`.
    Cancelled,
    /// The target was already running and could not be stopped.
    AlreadyStarted,
    /// No operation with that `user_data` is in flight.
    NotFound,
}

/// Cancels the in-flight operation with a given `user_data`.
pub struct CancelOp {
    target: u64,
}

impl Cancel {
    /// Cancel the operation whose [`Op::user_data`] is `target_user_data`.
    pub fn new(driver: &Driver, target_user_data: u64) -> Self {
        Op::submit_later(
            driver,
            CancelOp {
                target: target_user_data,
            },
        )
    }
}

impl sealed::Sealed for CancelOp {}

impl Operation for CancelOp {
    type Output = MeguriResult<CancelResult>;

    fn push(&mut self, sq: &mut SubmissionQueue, user_data: u64) -> bool {
        sq.push_cancel(user_data, self.target)
    }

    fn complete(self, result: MeguriResult<u32>) -> Self::Output {
        match result {
            Ok(_) => Ok(CancelResult::Cancelled),
            Err(MeguriError::Io(err)) => match err.raw_os_error() {
                Some(libc::ENOENT) => Ok(CancelResult::NotFound),
                Some(libc::EALREADY) => Ok(CancelResult::AlreadyStarted),
                _ => Err(MeguriError::Io(err)),
            },
            Err(err) => Err(err),
        }
    }
}
