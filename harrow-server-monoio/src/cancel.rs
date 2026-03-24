//! Cancellation-safe I/O utilities for io_uring.
//!
//! io_uring has a fundamental issue with Rust's async cancellation:
//! dropping a future does NOT cancel the in-flight kernel operation.
//! This can lead to use-after-free if the kernel writes to a buffer
//! after the future is dropped.
//!
//! This module provides safe wrappers using monoio's cancellable I/O traits.

use std::future::Future;
use std::io;

use monoio::io::{CancelHandle, CancelableAsyncReadRent, Canceller};

/// A wrapper around monoio's Canceller that provides a safe API
/// for timing out I/O operations.
#[allow(unused)] // Public API for future use
pub struct IoCanceller {
    inner: Canceller,
}

impl std::fmt::Debug for IoCanceller {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IoCanceller").finish()
    }
}

impl IoCanceller {
    /// Create a new canceller.
    pub fn new() -> Self {
        Self {
            inner: Canceller::new(),
        }
    }

    /// Get a handle to pass to I/O operations.
    #[allow(dead_code)]
    pub fn handle(&self) -> CancelHandle {
        self.inner.handle()
    }

    /// Cancel all in-flight operations associated with this canceller.
    /// Returns a new canceller that can be reused.
    #[allow(dead_code)]
    pub fn cancel(self) -> Self {
        Self {
            inner: self.inner.cancel(),
        }
    }
}

impl Default for IoCanceller {
    fn default() -> Self {
        Self::new()
    }
}

/// Result of a cancellable read operation.
#[allow(unused)] // Public API for future use
pub type CancellableReadResult<T> = (io::Result<usize>, T);

/// Read from a stream with a timeout, safely cancelling the operation
/// if the timeout expires.
#[allow(unused)] // Public API for future use
///
/// # Safety
/// This uses monoio's cancelable_read which properly cancels the kernel
/// operation when the timeout fires, preventing use-after-free.
///
/// # Cancellation Pattern
/// When the timeout fires:
/// 1. Call `canceller.cancel()` to submit cancellation to kernel
/// 2. Await the original read future to reclaim the buffer
/// 3. The operation completes with ECANCELED error
///
/// This ensures the buffer is not accessed by the kernel after we return.
pub async fn read_with_timeout<T>(
    stream: &mut impl CancelableAsyncReadRent,
    buf: T,
    timeout: std::time::Duration,
) -> CancellableReadResult<T>
where
    T: monoio::buf::IoBufMut,
{
    let canceller = Canceller::new();
    let handle = canceller.handle();

    let recv_fut = stream.cancelable_read(buf, handle);
    let mut recv_fut = std::pin::pin!(recv_fut);

    monoio::select! {
        result = &mut recv_fut => result,
        _ = monoio::time::sleep(timeout) => {
            // Timeout fired - submit cancellation to kernel
            let _ = canceller.cancel();
            // CRITICAL: Await the cancelled operation to reclaim the buffer
            let (_, buf) = recv_fut.await;
            (
                Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "read timeout",
                )),
                buf,
            )
        }
    }
}

/// Run a future with a timeout.
#[allow(dead_code)]
///
/// # Type Parameters
/// * `F` - The future to run
///
/// # Returns
/// * `Ok(T)` - If the future completes before the timeout
/// * `Err(TimeoutError)` - If the timeout fires
pub async fn timeout<F, T>(duration: std::time::Duration, future: F) -> Result<T, TimeoutError>
where
    F: Future<Output = T>,
{
    monoio::select! {
        result = future => Ok(result),
        _ = monoio::time::sleep(duration) => Err(TimeoutError),
    }
}

/// Error type indicating a timeout occurred.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub struct TimeoutError;

impl std::fmt::Display for TimeoutError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "operation timed out")
    }
}

impl std::error::Error for TimeoutError {}

#[cfg(test)]
mod tests {
    use super::*;
    use monoio::io::{AsyncWriteRentExt, CancelableAsyncReadRent};
    use monoio::net::{TcpListener, TcpStream};

    #[monoio::test]
    async fn test_canceller_basic() {
        let canceller = IoCanceller::new();
        let handle = canceller.handle();

        // Just verify we can create and clone handles
        let _handle2 = handle.clone();
        let _new_canceller = canceller.cancel();
    }

    #[monoio::test(enable_timer = true)]
    async fn test_timeout_success() {
        let result = timeout(std::time::Duration::from_secs(1), async { 42 }).await;

        assert_eq!(result, Ok(42));
    }

    #[monoio::test(enable_timer = true)]
    async fn test_timeout_elapsed() {
        let start = std::time::Instant::now();
        let result = timeout(
            std::time::Duration::from_millis(10),
            monoio::time::sleep(std::time::Duration::from_secs(1)),
        )
        .await;

        assert!(result.is_err());
        assert!(start.elapsed() < std::time::Duration::from_millis(100));
    }

    #[monoio::test(enable_timer = true)]
    async fn test_cancel_read() {
        // Bind a listener but don't accept - this will cause the connect to hang
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        // Connect in a separate task
        let connect_fut = async move { TcpStream::connect(addr).await };

        // Spawn the connection attempt
        let connect_handle = monoio::spawn(connect_fut);

        // Give it a moment to start connecting
        monoio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Accept the connection
        let (_server_stream, _) = listener.accept().await.unwrap();
        let mut client_stream = connect_handle.await.unwrap();

        // Now test cancellation on the client side
        let buf = vec![0u8; 1024];
        let canceller = Canceller::new();
        let handle = canceller.handle();

        // Spawn a task that cancels after 50ms
        monoio::spawn(async move {
            monoio::time::sleep(std::time::Duration::from_millis(50)).await;
            canceller.cancel();
        });

        // Start a read that will be cancelled
        let (res, buf) = client_stream.cancelable_read(buf, handle).await;

        // Should have been cancelled
        assert!(res.is_err());
        // Buffer should be returned
        assert_eq!(buf.len(), 1024);
    }

    #[monoio::test(enable_timer = true)]
    async fn test_read_with_timeout_cancelled() {
        // Bind a listener but don't accept - this will cause the connect to hang
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        // Connect in a separate task
        let connect_fut = async move { TcpStream::connect(addr).await };

        // Spawn the connection attempt
        let connect_handle = monoio::spawn(connect_fut);

        // Give it a moment to start connecting
        monoio::time::sleep(std::time::Duration::from_millis(10)).await;

        // Accept the connection
        let (_server_stream, _) = listener.accept().await.unwrap();
        let mut client_stream = connect_handle.await.unwrap();

        // Test read_with_timeout - should timeout and return buffer
        let buf = vec![0u8; 1024];
        let start = std::time::Instant::now();

        let (res, buf) = read_with_timeout(
            &mut client_stream,
            buf,
            std::time::Duration::from_millis(50),
        )
        .await;

        // Should have timed out
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().kind(), io::ErrorKind::TimedOut);
        // Buffer should be returned
        assert_eq!(buf.len(), 1024);
        // Should have taken around 50ms
        assert!(start.elapsed() >= std::time::Duration::from_millis(40));
    }

    #[monoio::test(enable_timer = true)]
    async fn test_read_with_timeout_success() {
        // Bind a listener
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();

        // Spawn server that sends data
        monoio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _ = stream.write_all(b"hello world").await;
        });

        // Connect client
        let mut client_stream = TcpStream::connect(addr).await.unwrap();

        // Test read_with_timeout - should succeed
        let buf = vec![0u8; 1024];
        let (res, buf) =
            read_with_timeout(&mut client_stream, buf, std::time::Duration::from_secs(1)).await;

        // Should have succeeded
        let n = res.expect("read should succeed");
        assert_eq!(&buf[..n], b"hello world");
    }
}
