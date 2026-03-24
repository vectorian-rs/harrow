//! Buffer pool for io_uring I/O operations.
//!
//! This module provides a userspace buffer pool to reduce allocator pressure
//! under high load. Future versions will use io_uring fixed buffers and
//! provided buffer rings for zero-copy operations.
//!
//! # Design
//!
//! The pool uses a thread-local stack (Vec) of pre-allocated buffers.
//! This is optimal for the thread-per-core model where each thread
//! manages its own connections.
//!
//! # Future Work
//!
//! When monoio supports it (or we use raw io_uring):
//! - `IORING_REGISTER_BUFFERS` for fixed buffers
//! - `IORING_SETUP_BUF_RING` for provided buffer rings
//! - True zero-copy receive with kernel-managed buffers

use std::cell::RefCell;

use bytes::BytesMut;

/// Default buffer size for reads (4 KiB).
///
/// This matches the page size on most systems and provides a good
/// balance between memory usage and read efficiency.
pub const DEFAULT_BUFFER_SIZE: usize = 4096;

/// Maximum number of buffers to keep in the pool per thread.
///
/// This limits memory usage under burst conditions. Additional
/// allocations will be freed rather than returned to the pool.
pub const MAX_POOL_SIZE: usize = 128;

thread_local! {
    static BUFFER_POOL: RefCell<Vec<BytesMut>> = const { RefCell::new(Vec::new()) };
}

/// Get a buffer from the pool or allocate a new one.
///
/// # Arguments
/// * `min_capacity` - Minimum capacity needed (defaults to DEFAULT_BUFFER_SIZE)
///
/// # Returns
/// A BytesMut with at least `min_capacity` capacity.
pub fn acquire_buffer(min_capacity: usize) -> BytesMut {
    let min_capacity = min_capacity.max(DEFAULT_BUFFER_SIZE);

    BUFFER_POOL.with(|pool| {
        let mut pool = pool.borrow_mut();

        // Try to find a buffer with sufficient capacity
        // Search from the end (most recently used = likely hot in cache)
        while let Some(buf) = pool.pop() {
            if buf.capacity() >= min_capacity {
                // Clear without deallocating
                let mut buf = buf;
                buf.clear();
                return buf;
            }
            // Buffer too small, let it drop and try next
        }

        // No suitable buffer in pool, allocate new
        BytesMut::with_capacity(min_capacity)
    })
}

/// Return a buffer to the pool.
///
/// The buffer is only kept if:
/// - The pool has space (under MAX_POOL_SIZE)
/// - The buffer has reasonable capacity (not oversized)
///
/// Otherwise, the buffer is dropped and its memory freed.
pub fn release_buffer(buf: BytesMut) {
    // Don't keep oversized buffers (more than 2x default)
    if buf.capacity() > DEFAULT_BUFFER_SIZE * 2 {
        return;
    }

    BUFFER_POOL.with(|pool| {
        let mut pool = pool.borrow_mut();

        if pool.len() < MAX_POOL_SIZE {
            pool.push(buf);
        }
        // Otherwise, drop the buffer (memory freed)
    });
}

/// Scoped buffer guard that returns buffer to pool on drop.
///
/// This ensures buffers are always returned to the pool, even if
/// operations fail or panic.
#[allow(dead_code)]
pub struct PooledBuffer {
    buf: Option<BytesMut>,
}

impl PooledBuffer {
    /// Acquire a new buffer from the pool.
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self {
            buf: Some(acquire_buffer(DEFAULT_BUFFER_SIZE)),
        }
    }

    /// Acquire a buffer with at least `min_capacity`.
    #[allow(dead_code)]
    pub fn with_capacity(min_capacity: usize) -> Self {
        Self {
            buf: Some(acquire_buffer(min_capacity)),
        }
    }

    /// Get a reference to the underlying BytesMut.
    #[allow(dead_code)]
    pub fn get(&self) -> &BytesMut {
        self.buf.as_ref().expect("buffer not stolen")
    }

    /// Get a mutable reference to the underlying BytesMut.
    #[allow(dead_code)]
    pub fn get_mut(&mut self) -> &mut BytesMut {
        self.buf.as_mut().expect("buffer not stolen")
    }

    /// Take ownership of the buffer, preventing it from being returned to pool.
    ///
    /// Use this when the buffer needs to outlive the guard (e.g., for body data).
    #[allow(dead_code)]
    pub fn take(&mut self) -> BytesMut {
        self.buf.take().expect("buffer already taken")
    }
}

impl Default for PooledBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for PooledBuffer {
    fn drop(&mut self) {
        if let Some(buf) = self.buf.take() {
            release_buffer(buf);
        }
    }
}

/// Reset the thread-local buffer pool.
///
/// Frees all pooled buffers. Useful for testing or memory-constrained
/// environments.
#[allow(dead_code)]
pub fn clear_pool() {
    BUFFER_POOL.with(|pool| {
        pool.borrow_mut().clear();
    });
}

/// Get current pool statistics.
///
/// Returns (pool_size, total_capacity) for the current thread.
#[allow(dead_code)]
pub fn pool_stats() -> (usize, usize) {
    BUFFER_POOL.with(|pool| {
        let pool = pool.borrow();
        let size = pool.len();
        let capacity: usize = pool.iter().map(|b| b.capacity()).sum();
        (size, capacity)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reset_pool() {
        clear_pool();
    }

    #[test]
    fn test_acquire_and_release() {
        reset_pool();

        // Acquire a buffer
        let buf = acquire_buffer(DEFAULT_BUFFER_SIZE);
        assert_eq!(buf.capacity(), DEFAULT_BUFFER_SIZE);

        // Release it back to pool
        release_buffer(buf);

        // Pool should have 1 buffer
        let (size, _) = pool_stats();
        assert_eq!(size, 1);

        // Acquire again - should get the same buffer back
        let buf2 = acquire_buffer(DEFAULT_BUFFER_SIZE);
        assert_eq!(buf2.capacity(), DEFAULT_BUFFER_SIZE);

        // Pool should be empty now
        let (size, _) = pool_stats();
        assert_eq!(size, 0);
    }

    #[test]
    fn test_pooled_buffer_guard() {
        reset_pool();

        {
            let guard = PooledBuffer::new();
            assert_eq!(guard.get().capacity(), DEFAULT_BUFFER_SIZE);
        }

        // Buffer should be back in pool
        let (size, _) = pool_stats();
        assert_eq!(size, 1);
    }

    #[test]
    fn test_pooled_buffer_take() {
        reset_pool();

        let mut guard = PooledBuffer::new();
        let buf = guard.take();

        // Buffer was taken, should not return to pool
        drop(guard);
        let (size, _) = pool_stats();
        assert_eq!(size, 0);

        // The taken buffer is owned by us
        drop(buf);
    }

    #[test]
    fn test_oversized_buffer_not_pooled() {
        reset_pool();

        // Allocate oversized buffer
        let buf = BytesMut::with_capacity(DEFAULT_BUFFER_SIZE * 10);
        release_buffer(buf);

        // Should not be in pool
        let (size, _) = pool_stats();
        assert_eq!(size, 0);
    }

    #[test]
    fn test_pool_size_limit() {
        reset_pool();

        // First, acquire many buffers without releasing
        let mut buffers = Vec::new();
        for _ in 0..MAX_POOL_SIZE + 10 {
            buffers.push(acquire_buffer(DEFAULT_BUFFER_SIZE));
        }

        // Now release them all - pool should fill to MAX_POOL_SIZE
        for buf in buffers {
            release_buffer(buf);
        }

        // Pool should be capped at MAX_POOL_SIZE
        let (size, _) = pool_stats();
        assert_eq!(size, MAX_POOL_SIZE);
    }

    #[test]
    fn test_buffer_cleared_on_acquire() {
        reset_pool();

        // Put a buffer with data in pool
        let mut buf = acquire_buffer(DEFAULT_BUFFER_SIZE);
        buf.extend_from_slice(b"hello world");
        release_buffer(buf);

        // Acquire again - should be empty
        let buf = acquire_buffer(DEFAULT_BUFFER_SIZE);
        assert!(buf.is_empty());
    }

    #[test]
    fn test_minimum_capacity() {
        reset_pool();

        // Request small buffer - should get at least DEFAULT_BUFFER_SIZE
        let buf = acquire_buffer(100);
        assert!(buf.capacity() >= DEFAULT_BUFFER_SIZE);
    }
}
