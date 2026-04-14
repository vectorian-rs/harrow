//! Runtime-agnostic I/O primitives for Harrow.
//!
//! Provides [`BufPool`] for thread-local buffer reuse across HTTP
//! connections. No runtime dependency — usable with tokio, monoio,
//! meguri, or any async runtime.

pub mod buf_pool;
pub use buf_pool::BufPool;
