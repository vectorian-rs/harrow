//! # Meguri (巡り) — A Pure, Safe, Tokio-Free io_uring Library
//!
//! Meguri is a low-level io_uring library designed for:
//!
//! - **Safety**: Correct buffer lifetime management. Operations own their buffers;
//!   dropping a future before completion never causes use-after-free.
//! - **Performance**: Direct access to the mmap'd rings with no runtime in
//!   between. Advanced features (registered buffers, fixed files, buffer rings,
//!   zero-copy send, multishot recv) are the goal, not yet implemented.
//! - **Runtime independence**: No Tokio dependency. Operations are plain
//!   futures woken through `Waker`; drive them with `Driver::block_on`, or call
//!   `Driver::turn` from any single-threaded event loop.
//! - **Linux-only**: io_uring is a Linux kernel feature. No kqueue/epoll pretense.
//!
//! ## Quick Start
//!
//! ```ignore
//! use bytes::BytesMut;
//! use meguri::{Driver, ops::{Accept, Recv}};
//!
//! let driver = Driver::new(256)?;
//! driver.block_on(async {
//!     let (socket, peer) = Accept::new(&driver, listener_fd).await?;
//!     let (n, buf) = Recv::new(&driver, socket.as_raw_fd(), BytesMut::with_capacity(4096), 0).await;
//!     // ...
//! });
//! ```
//!
//! ## Design
//!
//! Meguri has two layers:
//!
//! 1. **Raw Ring** (`Ring`, `SubmissionQueue`, `CompletionQueue`) — direct access
//!    to io_uring primitives with safe wrappers around unsafe syscalls. Use it
//!    to run your own event loop (as `harrow-server-meguri` does).
//! 2. **Operations** (`Driver`, `ops::*`) — `Accept`, `Connect`, `Recv`, `Send`,
//!    `Read`, `Write`, `Timeout` and `Cancel` as futures that own their buffers
//!    and are cancellation-safe. A `Driver` is the shared handle they submit
//!    through; `Driver::block_on` or `Driver::turn` drives completions.
//!
//! Registered resources (buffer rings, fixed files, registered buffers) are
//! planned but not implemented.
//!
//! ## Buffer Safety
//!
//! The core safety problem with io_uring in Rust is that the kernel may still be
//! reading/writing a buffer after the `Future` that submitted it has been dropped.
//! Meguri solves this by:
//!
//! 1. Every operation **owns** its buffer and returns it with the result.
//! 2. On `drop()` before completion, the buffer moves to the ring's
//!    **reclaim pool** and an `ASYNC_CANCEL` is queued for the operation.
//! 3. When the completion arrives, the buffer is released (an accepted socket
//!    is closed).
//!
//! This guarantees: **buffers are never freed while the kernel is using them**.

// io_uring is a Linux-only kernel feature. Fail fast on other platforms
// when the `io-uring` feature is enabled (which it is by default).
// Disable default features to compile meguri as an empty crate on non-Linux,
// allowing workspace-wide cargo commands to succeed.
#[cfg(all(feature = "io-uring", not(target_os = "linux")))]
compile_error!(
    "meguri requires Linux. io_uring is a Linux kernel feature and is not \
     available on macOS, Windows, or BSD. For cross-platform async I/O, \
     use the tokio or monoio backend instead. \
     To allow this crate to compile as an empty stub (e.g. for workspace-wide \
     cargo fmt/clippy on macOS), disable the default `io-uring` feature."
);

#[cfg(target_os = "linux")]
pub mod cq;
#[cfg(target_os = "linux")]
pub mod dispatcher;
#[cfg(target_os = "linux")]
pub mod driver;
#[cfg(target_os = "linux")]
pub mod error;
#[cfg(target_os = "linux")]
pub mod ops;
#[cfg(target_os = "linux")]
pub mod reclaim;
#[cfg(target_os = "linux")]
pub mod ring;
#[cfg(target_os = "linux")]
pub mod sq;
#[cfg(target_os = "linux")]
pub mod syscall;

#[cfg(target_os = "linux")]
pub use cq::CompletionQueue;
#[cfg(target_os = "linux")]
pub use dispatcher::Dispatcher;
#[cfg(target_os = "linux")]
pub use driver::Driver;
#[cfg(target_os = "linux")]
pub use error::{MeguriError, MeguriResult};
#[cfg(target_os = "linux")]
pub use reclaim::ReclaimPool;
#[cfg(target_os = "linux")]
pub use ring::Ring;
#[cfg(target_os = "linux")]
pub use sq::SubmissionQueue;
#[cfg(target_os = "linux")]
pub use syscall::{io_uring_enter, io_uring_register, io_uring_setup};
