//! Timeout operation (`IORING_OP_TIMEOUT`).

use std::time::Duration;

use super::{Op, Operation, Timeout, sealed};
use crate::driver::Driver;
use crate::error::{MeguriError, MeguriResult};
use crate::sq::SubmissionQueue;

/// Completes after a duration, measured by the kernel.
pub struct TimeoutOp {
    // Boxed: the kernel reads it asynchronously, so its address must not
    // change if the future moves. On 64-bit Linux `libc::timespec` has the
    // `__kernel_timespec` layout the kernel expects.
    ts: Box<libc::timespec>,
}

impl Timeout {
    /// Complete after `duration`.
    pub fn new(driver: &Driver, duration: Duration) -> Self {
        let ts = Box::new(libc::timespec {
            tv_sec: duration.as_secs() as libc::time_t,
            tv_nsec: duration.subsec_nanos().into(),
        });
        Op::submit_later(driver, TimeoutOp { ts })
    }
}

impl sealed::Sealed for TimeoutOp {}

impl Operation for TimeoutOp {
    type Output = MeguriResult<()>;

    fn push(&mut self, sq: &mut SubmissionQueue, user_data: u64) -> bool {
        // count = 0: a pure timer, not "after N other completions".
        sq.push_timeout(user_data, &raw const *self.ts, 0, 0)
    }

    fn complete(self, result: MeguriResult<u32>) -> Self::Output {
        match result {
            // The kernel reports an expired timer as -ETIME.
            Err(MeguriError::Io(err)) if err.raw_os_error() == Some(libc::ETIME) => Ok(()),
            other => other.map(|_| ()),
        }
    }
}
