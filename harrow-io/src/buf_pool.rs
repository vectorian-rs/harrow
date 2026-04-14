//! Thread-local buffer pool for HTTP connections.
//!
//! Reuses `BytesMut` buffers across requests on the same worker thread,
//! eliminating per-request heap allocations. Follows the ntex model:
//! separate read/write pools with watermark-based sizing.

use bytes::BytesMut;
use std::cell::RefCell;

const LOW_WATERMARK: usize = 512;
const HIGH_WATERMARK: usize = 16 * 1024;
const MAX_POOL_SIZE: usize = 128;
const DEFAULT_CAPACITY: usize = 8 * 1024;

struct Pool {
    read: Vec<BytesMut>,
    write: Vec<BytesMut>,
}

thread_local! {
    static POOL: RefCell<Pool> = const { RefCell::new(Pool {
        read: Vec::new(),
        write: Vec::new(),
    }) };
}

fn acquire(f: fn(&mut Pool) -> &mut Vec<BytesMut>) -> BytesMut {
    POOL.with(|pool| {
        let mut p = pool.borrow_mut();
        f(&mut p)
            .pop()
            .unwrap_or_else(|| BytesMut::with_capacity(DEFAULT_CAPACITY))
    })
}

fn release(mut buf: BytesMut, f: fn(&mut Pool) -> &mut Vec<BytesMut>) {
    if !(LOW_WATERMARK..=HIGH_WATERMARK).contains(&buf.capacity()) {
        return;
    }
    buf.clear();
    POOL.with(|pool| {
        let mut p = pool.borrow_mut();
        let v = f(&mut p);
        if v.len() < MAX_POOL_SIZE {
            v.push(buf);
        }
    });
}

/// Thread-local buffer pool.
///
/// All methods are `'static` — the pool is per-thread, no synchronization.
pub struct BufPool;

impl BufPool {
    pub fn acquire_read() -> BytesMut {
        acquire(|p| &mut p.read)
    }

    pub fn acquire_write() -> BytesMut {
        acquire(|p| &mut p.write)
    }

    pub fn release_read(buf: BytesMut) {
        release(buf, |p| &mut p.read);
    }

    pub fn release_write(buf: BytesMut) {
        release(buf, |p| &mut p.write);
    }

    #[cfg(test)]
    fn pool_size(f: fn(&Pool) -> usize) -> usize {
        POOL.with(|pool| {
            let p = pool.borrow();
            f(&p)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn acquire_returns_default_capacity() {
        let buf = BufPool::acquire_read();
        assert_eq!(buf.capacity(), DEFAULT_CAPACITY);
        assert_eq!(buf.len(), 0);
    }

    #[test]
    fn release_and_reacquire_reuses_buffer() {
        let buf = BufPool::acquire_read();
        let ptr = buf.as_ptr();
        let cap = buf.capacity();
        BufPool::release_read(buf);

        let buf2 = BufPool::acquire_read();
        assert_eq!(buf2.as_ptr(), ptr);
        assert_eq!(buf2.capacity(), cap);
        assert_eq!(buf2.len(), 0);
    }

    #[test]
    fn release_clears_data() {
        let mut buf = BufPool::acquire_write();
        buf.extend_from_slice(b"hello world");
        assert_eq!(buf.len(), 11);

        BufPool::release_write(buf);
        let buf2 = BufPool::acquire_write();
        assert_eq!(buf2.len(), 0);
    }

    #[test]
    fn undersized_buffer_not_pooled() {
        let buf = BytesMut::with_capacity(64);
        BufPool::release_read(buf);
        assert_eq!(BufPool::pool_size(|p| p.read.len()), 0);
    }

    #[test]
    fn oversized_buffer_not_pooled() {
        let buf = BytesMut::with_capacity(32 * 1024);
        BufPool::release_read(buf);
        assert_eq!(BufPool::pool_size(|p| p.read.len()), 0);
    }

    #[test]
    fn pool_bounded_at_max_size() {
        for _ in 0..MAX_POOL_SIZE + 10 {
            let buf = BytesMut::with_capacity(DEFAULT_CAPACITY);
            BufPool::release_read(buf);
        }
        assert_eq!(BufPool::pool_size(|p| p.read.len()), MAX_POOL_SIZE);
    }

    #[test]
    fn read_and_write_pools_independent() {
        let buf = BufPool::acquire_read();
        BufPool::release_read(buf);
        assert_eq!(BufPool::pool_size(|p| p.read.len()), 1);
        assert_eq!(BufPool::pool_size(|p| p.write.len()), 0);

        let buf = BufPool::acquire_write();
        BufPool::release_write(buf);
        assert_eq!(BufPool::pool_size(|p| p.write.len()), 1);
    }
}
