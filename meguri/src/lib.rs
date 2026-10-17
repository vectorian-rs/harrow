//! # Meguri (巡り) — A Pure, Safe, Tokio-Free io_uring Library
//!
//! Meguri is a low-level io_uring library designed for:
//!
//! - **Safety**: Correct buffer lifetime management. Operations own their buffers;
//!   dropping a future before completion never causes use-after-free.
//! - **Performance**: Exposes advanced io_uring features (registered buffers, fixed
//!   files, buffer rings, zero-copy send, multishot recv) that other runtimes omit.
//! - **Runtime independence**: No Tokio dependency. Uses eventfd + `Waker` for
//!   async integration. Works standalone or with any async runtime.
//! - **Linux-only**: io_uring is a Linux kernel feature. No kqueue/epoll pretense.
//!
//! ## Quick Start
//!
//! ```ignore
//! use meguri::Ring;
//!
//! let mut ring = Ring::new(256)?;
//! // Submit operations, poll completions, wake tasks.
//! ```
//!
//! ## Design
//!
//! Meguri has three layers:
//!
//! 1. **Raw Ring** (`Ring`, `SubmissionQueue`, `CompletionQueue`) — direct access
//!    to io_uring primitives with safe wrappers around unsafe syscalls.
//! 2. **Safe Operations** (`ops::*`) — typed I/O operations (`Read`, `Write`,
//!    `Accept`, etc.) that own their buffers and are cancellation-safe.
//! 3. **Advanced Features** (`BufRing`, `FixedFileRegistry`, `RegisteredBufferPool`)
//!    — registered resources for maximum throughput.
//!
//! ## Buffer Safety
//!
//! The core safety problem with io_uring in Rust is that the kernel may still be
//! reading/writing a buffer after the `Future` that submitted it has been dropped.
//! Meguri solves this by:
//!
//! 1. Every operation **owns** its buffer.
//! 2. On `drop()` before completion, the operation registers a **reclaim callback**.
//! 3. When the completion arrives, the buffer is returned to a reclaim pool.
//! 4. The reclaim pool holds buffers until the ring is destroyed.
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
