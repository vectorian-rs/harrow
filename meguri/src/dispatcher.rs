//! Dispatcher: maps user_data to wakers, completion results, and reclaim
//! actions.
//!
//! When an operation is submitted, its `user_data` is registered here. When
//! the completion arrives, the dispatcher stores the result and wakes the
//! task, which collects it with [`Dispatcher::take_completion`]. If the
//! operation was dropped before completion, the dispatcher routes its
//! resources to the reclaim pool instead.

use std::task::Waker;

use slab::Slab;

use crate::reclaim::{ReclaimPool, Reclaimable};

/// State for a registered operation.
pub enum OpState {
    /// Operation is pending completion. Waker will be called on completion.
    Pending { waker: Option<Waker> },
    /// The kernel completed the operation; the owner has not collected it.
    Completed { res: i32, flags: u32 },
    /// Operation was dropped before completion. The buffer must be reclaimed.
    Dropped { reclaim_key: u64 },
    /// Submitted by meguri itself (e.g. a cancel issued on drop); the
    /// completion is discarded.
    Internal,
}

/// Maps user_data (slab key) to operation state.
///
/// Uses a `Slab` for O(1) register/complete/lookup — no hashing overhead
/// on the hot completion path.
pub struct Dispatcher {
    pending: Slab<OpState>,
    /// Entries still waiting for a CQE (everything except `Completed`).
    in_flight: usize,
}

impl Default for Dispatcher {
    fn default() -> Self {
        Self::new()
    }
}

impl Dispatcher {
    pub fn new() -> Self {
        Self {
            pending: Slab::with_capacity(256),
            in_flight: 0,
        }
    }

    /// Allocate a new user_data and register the operation as pending.
    /// Returns the user_data (slab key) to attach to the SQE.
    pub fn register(&mut self, waker: Option<Waker>) -> u64 {
        self.in_flight += 1;
        self.pending.insert(OpState::Pending { waker }) as u64
    }

    /// Allocate a user_data whose completion is discarded.
    pub fn register_internal(&mut self) -> u64 {
        self.in_flight += 1;
        self.pending.insert(OpState::Internal) as u64
    }

    /// Remove a registration whose SQE was never pushed.
    pub fn unregister(&mut self, user_data: u64) {
        let key = user_data as usize;
        if let Some(state) = self.pending.try_remove(key)
            && !matches!(state, OpState::Completed { .. })
        {
            self.in_flight -= 1;
        }
    }

    /// Mark an operation as dropped (future dropped before completion).
    /// The buffer will be reclaimed when the completion arrives.
    pub fn mark_dropped(&mut self, user_data: u64, reclaim_key: u64) {
        let key = user_data as usize;
        if let Some(state) = self.pending.get_mut(key) {
            *state = OpState::Dropped { reclaim_key };
        }
    }

    /// Release an operation whose owner is going away.
    ///
    /// If the kernel already completed it, the entry is removed and
    /// `resources` are dropped now. Otherwise `resources` move to the
    /// reclaim pool until the completion arrives. Returns `true` if the
    /// operation is still in flight in the kernel.
    pub fn release(
        &mut self,
        user_data: u64,
        resources: impl Reclaimable,
        reclaim: &mut ReclaimPool,
    ) -> bool {
        let key = user_data as usize;
        match self.pending.get(key) {
            Some(OpState::Pending { .. }) => {
                let reclaim_key = reclaim.register(resources);
                self.mark_dropped(user_data, reclaim_key);
                true
            }
            Some(OpState::Completed { res, .. }) => {
                let res = *res;
                self.pending.remove(key);
                Box::new(resources).reclaim(res);
                false
            }
            _ => false,
        }
    }

    /// Process a completion. Stores the result and wakes the task if
    /// pending, or reclaims the buffer if the operation was dropped.
    pub fn complete(&mut self, user_data: u64, res: i32, flags: u32, reclaim: &mut ReclaimPool) {
        let key = user_data as usize;
        let Some(state) = self.pending.get_mut(key) else {
            // Completion for an unknown user_data — the caller pushed an SQE
            // without registering it here.
            return;
        };
        match state {
            OpState::Pending { waker } => {
                let waker = waker.take();
                *state = OpState::Completed { res, flags };
                self.in_flight -= 1;
                if let Some(w) = waker {
                    w.wake();
                }
            }
            OpState::Dropped { reclaim_key } => {
                let reclaim_key = *reclaim_key;
                self.pending.remove(key);
                self.in_flight -= 1;
                reclaim.reclaim(reclaim_key, res);
            }
            OpState::Internal => {
                self.pending.remove(key);
                self.in_flight -= 1;
            }
            // A second CQE for the same user_data (multishot); not used yet.
            OpState::Completed { .. } => {}
        }
    }

    /// Take the result of a completed operation, freeing its user_data.
    /// Returns `None` while the operation is still pending.
    pub fn take_completion(&mut self, user_data: u64) -> Option<(i32, u32)> {
        let key = user_data as usize;
        match self.pending.get(key) {
            Some(OpState::Completed { res, flags }) => {
                let result = (*res, *flags);
                self.pending.remove(key);
                Some(result)
            }
            _ => None,
        }
    }

    /// Update the waker for a pending operation (called from Future::poll).
    pub fn update_waker(&mut self, user_data: u64, waker: &Waker) {
        let key = user_data as usize;
        if let Some(OpState::Pending { waker: stored }) = self.pending.get_mut(key) {
            match stored {
                Some(existing) if existing.will_wake(waker) => {}
                _ => *stored = Some(waker.clone()),
            }
        }
    }

    /// Check if an operation is still registered (pending, completed but not
    /// collected, or dropped and awaiting reclaim).
    pub fn is_pending(&self, user_data: u64) -> bool {
        self.pending.contains(user_data as usize)
    }

    /// Number of operations still waiting for a completion from the kernel.
    pub fn in_flight(&self) -> usize {
        self.in_flight
    }

    /// Number of registered operations.
    pub fn len(&self) -> usize {
        self.pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicI32, Ordering};

    use super::*;

    struct Probe(Arc<AtomicI32>);

    impl Reclaimable for Probe {
        fn reclaim(self: Box<Self>, result: i32) {
            self.0.store(result, Ordering::SeqCst);
        }
    }

    #[test]
    fn completion_is_stored_until_taken() {
        let mut dispatcher = Dispatcher::new();
        let mut reclaim = ReclaimPool::new();
        let ud = dispatcher.register(None);

        assert_eq!(dispatcher.take_completion(ud), None);
        assert_eq!(dispatcher.in_flight(), 1);
        dispatcher.complete(ud, 7, 1, &mut reclaim);
        assert_eq!(dispatcher.in_flight(), 0);
        assert_eq!(dispatcher.take_completion(ud), Some((7, 1)));
        assert!(dispatcher.is_empty());
    }

    #[test]
    fn release_before_completion_reclaims_on_completion() {
        let mut dispatcher = Dispatcher::new();
        let mut reclaim = ReclaimPool::new();
        let seen = Arc::new(AtomicI32::new(i32::MIN));
        let ud = dispatcher.register(None);

        assert!(dispatcher.release(ud, Probe(Arc::clone(&seen)), &mut reclaim));
        assert_eq!(reclaim.len(), 1);
        dispatcher.complete(ud, 3, 0, &mut reclaim);
        assert_eq!(seen.load(Ordering::SeqCst), 3);
        assert!(reclaim.is_empty());
        assert!(dispatcher.is_empty());
    }

    #[test]
    fn release_after_completion_reclaims_immediately() {
        let mut dispatcher = Dispatcher::new();
        let mut reclaim = ReclaimPool::new();
        let seen = Arc::new(AtomicI32::new(i32::MIN));
        let ud = dispatcher.register(None);
        dispatcher.complete(ud, 5, 0, &mut reclaim);

        assert!(!dispatcher.release(ud, Probe(Arc::clone(&seen)), &mut reclaim));
        assert_eq!(seen.load(Ordering::SeqCst), 5);
        assert!(reclaim.is_empty());
        assert!(dispatcher.is_empty());
    }

    #[test]
    fn internal_completions_are_discarded() {
        let mut dispatcher = Dispatcher::new();
        let mut reclaim = ReclaimPool::new();
        let ud = dispatcher.register_internal();
        dispatcher.complete(ud, 0, 0, &mut reclaim);
        assert!(dispatcher.is_empty());
    }
}
