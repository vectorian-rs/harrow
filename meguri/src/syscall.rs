//! Raw io_uring syscall wrappers.
//!
//! These are thin unsafe wrappers around the three io_uring syscalls:
//! - `io_uring_setup` — create a ring
//! - `io_uring_enter` — submit SQEs and/or wait for CQEs
//! - `io_uring_register` — register files, buffers, eventfd, etc.
//!
//! # Safety
//!
//! Each function documents its safety requirements. The safe API (`Ring`)
//! enforces these invariants.

use std::os::fd::RawFd;

/// Raw syscall wrapper for Linux (x86_64).
///
/// # Safety
/// The caller must ensure the syscall number and arguments are valid.
#[cfg(target_arch = "x86_64")]
#[inline]
unsafe fn raw_syscall(
    n: libc::c_long,
    a1: libc::c_ulong,
    a2: libc::c_ulong,
    a3: libc::c_ulong,
    a4: libc::c_ulong,
    a5: libc::c_ulong,
    a6: libc::c_ulong,
) -> libc::c_long {
    let ret: libc::c_long;
    // SAFETY: the caller upholds the syscall's contract (see `# Safety`).
    unsafe {
        std::arch::asm!(
            "syscall",
            in("rax") n,
            in("rdi") a1,
            in("rsi") a2,
            in("rdx") a3,
            in("r10") a4,
            in("r8") a5,
            in("r9") a6,
            out("rcx") _,  // syscall clobbers rcx
            out("r11") _,  // syscall clobbers r11
            lateout("rax") ret,
            options(nostack),
        );
    }
    ret
}

/// Raw syscall wrapper for Linux (aarch64 / Graviton).
///
/// # Safety
/// The caller must ensure the syscall number and arguments are valid.
#[cfg(target_arch = "aarch64")]
#[inline]
unsafe fn raw_syscall(
    n: libc::c_long,
    a1: libc::c_ulong,
    a2: libc::c_ulong,
    a3: libc::c_ulong,
    a4: libc::c_ulong,
    a5: libc::c_ulong,
    a6: libc::c_ulong,
) -> libc::c_long {
    let ret: libc::c_long;
    unsafe {
        std::arch::asm!(
            "svc #0",
            in("x8") n,
            inlateout("x0") a1 => ret,
            in("x1") a2,
            in("x2") a3,
            in("x3") a4,
            in("x4") a5,
            in("x5") a6,
            options(nostack),
        );
    }
    ret
}

// ---------------------------------------------------------------------------
// io_uring_setup
// ---------------------------------------------------------------------------

/// Parameters for `io_uring_setup`.
///
/// Mirrors the kernel's `struct io_uring_params`.
#[repr(C)]
#[derive(Debug, Clone, Default)]
pub struct IoUringParams {
    pub sq_entries: u32,
    pub cq_entries: u32,
    pub flags: u32,
    pub sq_thread_cpu: u32,
    pub sq_thread_idle: u32,
    pub features: u32,
    pub wq_fd: u32,
    pub resv: [u32; 3],
    pub sq_off: IoSqringOffsets,
    pub cq_off: IoCqringOffsets,
}

#[repr(C)]
#[derive(Debug, Clone, Default)]
pub struct IoSqringOffsets {
    pub head: u32,
    pub tail: u32,
    pub ring_mask: u32,
    pub ring_entries: u32,
    pub flags: u32,
    pub dropped: u32,
    pub array: u32,
    pub resv1: u32,
    pub user_addr: u64,
}

#[repr(C)]
#[derive(Debug, Clone, Default)]
pub struct IoCqringOffsets {
    pub head: u32,
    pub tail: u32,
    pub ring_mask: u32,
    pub ring_entries: u32,
    pub overflow: u32,
    pub cqes: u32,
    pub flags: u32,
    pub resv1: u32,
    pub user_addr: u64,
}

// ---------------------------------------------------------------------------
// io_uring_setup flags
// ---------------------------------------------------------------------------

pub const IORING_SETUP_IOPOLL: u32 = 1 << 0;
pub const IORING_SETUP_SQPOLL: u32 = 1 << 1;
pub const IORING_SETUP_SQ_AFF: u32 = 1 << 2;
pub const IORING_SETUP_CQSIZE: u32 = 1 << 3;
pub const IORING_SETUP_CLAMP: u32 = 1 << 4;
pub const IORING_SETUP_ATTACH_WQ: u32 = 1 << 5;
pub const IORING_SETUP_R_DISABLED: u32 = 1 << 6;
pub const IORING_SETUP_SUBMIT_ALL: u32 = 1 << 7;
pub const IORING_SETUP_COOP_TASKRUN: u32 = 1 << 8;
pub const IORING_SETUP_TASKRUN_FLAG: u32 = 1 << 9;
pub const IORING_SETUP_SQE128: u32 = 1 << 10;
pub const IORING_SETUP_CQE32: u32 = 1 << 11;
pub const IORING_SETUP_SINGLE_ISSUER: u32 = 1 << 12;
pub const IORING_SETUP_DEFER_TASKRUN: u32 = 1 << 13;
pub const IORING_SETUP_NO_MMAP: u32 = 1 << 14;
pub const IORING_SETUP_REGISTERED_FD_ONLY: u32 = 1 << 15;
pub const IORING_SETUP_NO_SQARRAY: u32 = 1 << 16;

// ---------------------------------------------------------------------------
// io_uring_setup features (returned in params.features)
// ---------------------------------------------------------------------------

pub const IORING_FEAT_SINGLE_MMAP: u32 = 1 << 0;
pub const IORING_FEAT_NODROP: u32 = 1 << 1;
pub const IORING_FEAT_SUBMIT_STABLE: u32 = 1 << 2;
pub const IORING_FEAT_RW_CUR_POS: u32 = 1 << 3;
pub const IORING_FEAT_CUR_PERSONALITY: u32 = 1 << 4;
pub const IORING_FEAT_FAST_POLL: u32 = 1 << 5;
pub const IORING_FEAT_POLL_32BITS: u32 = 1 << 6;
pub const IORING_FEAT_SQPOLL_NONFIXED: u32 = 1 << 7;
pub const IORING_FEAT_EXT_ARG: u32 = 1 << 8;
pub const IORING_FEAT_NATIVE_WORKERS: u32 = 1 << 9;
pub const IORING_FEAT_RSRC_TAGS: u32 = 1 << 10;
pub const IORING_FEAT_CQE_SKIP: u32 = 1 << 11;
pub const IORING_FEAT_LINKED_FILE: u32 = 1 << 12;
pub const IORING_FEAT_REG_REG_RING: u32 = 1 << 13;

// ---------------------------------------------------------------------------
// io_uring_register opcodes
// ---------------------------------------------------------------------------

pub const IORING_REGISTER_BUFFERS: u32 = 0;
pub const IORING_UNREGISTER_BUFFERS: u32 = 1;
pub const IORING_REGISTER_FILES: u32 = 2;
pub const IORING_UNREGISTER_FILES: u32 = 3;
pub const IORING_REGISTER_EVENTFD: u32 = 4;
pub const IORING_UNREGISTER_EVENTFD: u32 = 5;
pub const IORING_REGISTER_FILES_UPDATE: u32 = 6;
pub const IORING_REGISTER_EVENTFD_ASYNC: u32 = 7;
pub const IORING_REGISTER_PROBE: u32 = 8;
pub const IORING_REGISTER_PERSONALITY: u32 = 9;
pub const IORING_UNREGISTER_PERSONALITY: u32 = 10;
pub const IORING_REGISTER_RESTRICTIONS: u32 = 11;
pub const IORING_REGISTER_ENABLE_RINGS: u32 = 12;
pub const IORING_REGISTER_FILES2: u32 = 13;
pub const IORING_REGISTER_FILES_UPDATE2: u32 = 14;
pub const IORING_REGISTER_BUFFERS2: u32 = 15;
pub const IORING_REGISTER_BUFFERS_UPDATE: u32 = 16;
pub const IORING_REGISTER_IOWQ_AFF: u32 = 17;
pub const IORING_UNREGISTER_IOWQ_AFF: u32 = 18;
pub const IORING_REGISTER_IOWQ_MAX_WORKERS: u32 = 19;
pub const IORING_REGISTER_RING_FDS: u32 = 20;
pub const IORING_UNREGISTER_RING_FDS: u32 = 21;
pub const IORING_REGISTER_PBUF_RING: u32 = 22;
pub const IORING_UNREGISTER_PBUF_RING: u32 = 23;
pub const IORING_REGISTER_SYNC_CANCEL: u32 = 24;
pub const IORING_REGISTER_FILE_ALLOC_RANGE: u32 = 25;
pub const IORING_REGISTER_PBUF_STATUS: u32 = 26;
pub const IORING_REGISTER_NAPI: u32 = 27;
pub const IORING_UNREGISTER_NAPI: u32 = 28;
pub const IORING_REGISTER_SEND_MSG_RING: u32 = 29;
pub const IORING_REGISTER_ZCRX_SEND_IFQ: u32 = 30;
pub const IORING_REGISTER_CLONE_BUFFERS: u32 = 31;

// ---------------------------------------------------------------------------
// SQE flags
// ---------------------------------------------------------------------------

pub const IOSQE_FIXED_FILE: u8 = 1 << 0;
pub const IOSQE_IO_DRAIN: u8 = 1 << 1;
pub const IOSQE_IO_LINK: u8 = 1 << 2;
pub const IOSQE_IO_HARDLINK: u8 = 1 << 3;
pub const IOSQE_ASYNC: u8 = 1 << 4;
pub const IOSQE_BUFFER_SELECT: u8 = 1 << 5;
pub const IOSQE_CQE_SKIP_SUCCESS: u8 = 1 << 6;

// ---------------------------------------------------------------------------
// CQE flags
// ---------------------------------------------------------------------------

pub const IORING_CQE_F_BUFFER: u32 = 1 << 0;
pub const IORING_CQE_F_MORE: u32 = 1 << 1;
pub const IORING_CQE_F_SOCK_NONEMPTY: u32 = 1 << 2;
pub const IORING_CQE_F_NOTIF: u32 = 1 << 3;

// ---------------------------------------------------------------------------
// Opcodes
// ---------------------------------------------------------------------------

pub const IORING_OP_NOP: u8 = 0;
pub const IORING_OP_READV: u8 = 1;
pub const IORING_OP_WRITEV: u8 = 2;
pub const IORING_OP_FSYNC: u8 = 3;
pub const IORING_OP_READ_FIXED: u8 = 4;
pub const IORING_OP_WRITE_FIXED: u8 = 5;
pub const IORING_OP_POLL_ADD: u8 = 6;
pub const IORING_OP_POLL_REMOVE: u8 = 7;
pub const IORING_OP_SYNC_FILE_RANGE: u8 = 8;
pub const IORING_OP_SENDMSG: u8 = 9;
pub const IORING_OP_RECVMSG: u8 = 10;
pub const IORING_OP_TIMEOUT: u8 = 11;
pub const IORING_OP_TIMEOUT_REMOVE: u8 = 12;
pub const IORING_OP_ACCEPT: u8 = 13;
pub const IORING_OP_ASYNC_CANCEL: u8 = 14;
pub const IORING_OP_LINK_TIMEOUT: u8 = 15;
pub const IORING_OP_CONNECT: u8 = 16;
pub const IORING_OP_FALLOCATE: u8 = 17;
pub const IORING_OP_OPENAT: u8 = 18;
pub const IORING_OP_CLOSE: u8 = 19;
pub const IORING_OP_FILES_UPDATE: u8 = 20;
pub const IORING_OP_STATX: u8 = 21;
pub const IORING_OP_READ: u8 = 22;
pub const IORING_OP_WRITE: u8 = 23;
pub const IORING_OP_FADVISE: u8 = 24;
pub const IORING_OP_MADVISE: u8 = 25;
pub const IORING_OP_SEND: u8 = 26;
pub const IORING_OP_RECV: u8 = 27;
pub const IORING_OP_OPENAT2: u8 = 28;
pub const IORING_OP_EPOLL_CTL: u8 = 29;
pub const IORING_OP_SPLICE: u8 = 30;
pub const IORING_OP_PROVIDE_BUFFERS: u8 = 31;
pub const IORING_OP_REMOVE_BUFFERS: u8 = 32;
pub const IORING_OP_TEE: u8 = 33;
pub const IORING_OP_SHUTDOWN: u8 = 34;
pub const IORING_OP_RENAMEAT: u8 = 35;
pub const IORING_OP_UNLINKAT: u8 = 36;
pub const IORING_OP_MKDIRAT: u8 = 37;
pub const IORING_OP_SYMLINKAT: u8 = 38;
pub const IORING_OP_LINKAT: u8 = 39;
pub const IORING_OP_MSG_RING: u8 = 40;
pub const IORING_OP_FSETXATTR: u8 = 41;
pub const IORING_OP_SETXATTR: u8 = 42;
pub const IORING_OP_FGETXATTR: u8 = 43;
pub const IORING_OP_GETXATTR: u8 = 44;
pub const IORING_OP_SOCKET: u8 = 45;
pub const IORING_OP_URING_CMD: u8 = 46;
pub const IORING_OP_SEND_ZC: u8 = 47;
pub const IORING_OP_SENDMSG_ZC: u8 = 48;
pub const IORING_OP_READ_MULTISHOT: u8 = 49;
pub const IORING_OP_WAITID: u8 = 50;
pub const IORING_OP_FUTEX_WAIT: u8 = 51;
pub const IORING_OP_FUTEX_WAKE: u8 = 52;
pub const IORING_OP_FUTEX_WAITV: u8 = 53;
pub const IORING_OP_FIXED_FD_INSTALL: u8 = 54;
pub const IORING_OP_FTRUNCATE: u8 = 55;
pub const IORING_OP_BIND: u8 = 56;
pub const IORING_OP_UNLINKAT2: u8 = 57;

// ---------------------------------------------------------------------------
// SQE layout (128 bytes for IORING_SETUP_SQE128, 64 bytes standard)
// ---------------------------------------------------------------------------

/// Standard 64-byte SQE.
///
/// Mirrors the kernel's `struct io_uring_sqe`. Many fields are unions in the
/// kernel struct; we expose the most common variant and let callers reinterpret
/// via the same offset when needed (e.g. `splice_fd_in` and `file_index` share
/// offset 44).
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IoUringSqe {
    pub opcode: u8,
    pub flags: u8,
    pub ioprio: u16,
    pub fd: i32,
    pub off: u64,
    pub addr: u64,
    pub len: u32,
    pub rw_flags: u32,
    pub user_data: u64,
    pub buf_index: u16,
    pub personality: u16,
    pub splice_fd_in: i32,
    pub addr3: u64,
    pub __pad2: u64,
}

impl IoUringSqe {
    /// Create a zeroed SQE.
    pub const fn zeroed() -> Self {
        Self {
            opcode: 0,
            flags: 0,
            ioprio: 0,
            fd: 0,
            off: 0,
            addr: 0,
            len: 0,
            rw_flags: 0,
            user_data: 0,
            buf_index: 0,
            personality: 0,
            splice_fd_in: 0,
            addr3: 0,
            __pad2: 0,
        }
    }
}

/// CQE layout (16 bytes standard, 32 bytes with IORING_SETUP_CQE32).
///
/// We use the standard 16-byte layout. CQE32 is not supported.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct IoUringCqe {
    pub user_data: u64,
    pub res: i32,
    pub flags: u32,
}

// ---------------------------------------------------------------------------
// mmap offsets for io_uring ring fd
// ---------------------------------------------------------------------------

pub const IORING_OFF_SQ_RING: u64 = 0;
pub const IORING_OFF_CQ_RING: u64 = 0x8000000;
pub const IORING_OFF_SQES: u64 = 0x10000000;

// ---------------------------------------------------------------------------
// io_uring_enter flags
// ---------------------------------------------------------------------------

pub const IORING_ENTER_GETEVENTS: u32 = 1 << 0;
pub const IORING_ENTER_SQ_WAKEUP: u32 = 1 << 1;
pub const IORING_ENTER_SQ_WAIT: u32 = 1 << 2;
pub const IORING_ENTER_EXT_ARG: u32 = 1 << 3;
pub const IORING_ENTER_REGISTERED_RING: u32 = 1 << 4;

// ---------------------------------------------------------------------------
// SQ ring flags (read from kernel-mapped sq_flags)
// ---------------------------------------------------------------------------

pub const IORING_SQ_NEED_WAKEUP: u32 = 1 << 0;
pub const IORING_SQ_CQ_OVERFLOW: u32 = 1 << 1;
pub const IORING_SQ_TASKRUN: u32 = 1 << 2;

// ---------------------------------------------------------------------------
// Syscall wrappers
// ---------------------------------------------------------------------------

/// Create an io_uring instance.
///
/// # Safety
/// - `entries` must be a power of 2 (or the kernel will clamp it).
/// - The returned fd must be stored and used for subsequent `io_uring_enter`
///   and `io_uring_register` calls.
/// - The caller is responsible for mmap'ing the SQ and CQ regions using the
///   offsets returned in `params`.
pub unsafe fn io_uring_setup(entries: u32, params: &mut IoUringParams) -> std::io::Result<RawFd> {
    let ret = unsafe {
        raw_syscall(
            libc::SYS_io_uring_setup,
            entries as libc::c_ulong,
            params as *mut IoUringParams as libc::c_ulong,
            0,
            0,
            0,
            0,
        )
    };
    if ret < 0 {
        Err(std::io::Error::from_raw_os_error(-ret as i32))
    } else {
        Ok(ret as RawFd)
    }
}

/// Submit SQEs and/or wait for CQEs.
///
/// # Safety
/// - `ring_fd` must be a valid io_uring fd.
/// - If `to_submit > 0`, the SQ must have valid SQEs pushed.
/// - If `sigmask` is non-null, it must point to a valid `sigset_t` or the
///   kernel may read invalid memory.
pub unsafe fn io_uring_enter(
    ring_fd: RawFd,
    to_submit: u32,
    min_complete: u32,
    flags: u32,
    sigmask: *const libc::sigset_t,
    sigmask_sz: usize,
) -> std::io::Result<u32> {
    let ret = unsafe {
        raw_syscall(
            libc::SYS_io_uring_enter,
            ring_fd as libc::c_ulong,
            to_submit as libc::c_ulong,
            min_complete as libc::c_ulong,
            flags as libc::c_ulong,
            sigmask as libc::c_ulong,
            sigmask_sz as libc::c_ulong,
        )
    };
    if ret < 0 {
        Err(std::io::Error::from_raw_os_error(-ret as i32))
    } else {
        Ok(ret as u32)
    }
}

/// Register resources with an io_uring instance.
///
/// # Safety
/// - `ring_fd` must be a valid io_uring fd.
/// - `arg` and `nr_args` must match the expectations for `opcode`.
///   For example, `IORING_REGISTER_BUFFERS` expects `arg` to be a pointer
///   to an array of `iovec` and `nr_args` to be the array length.
pub unsafe fn io_uring_register(
    ring_fd: RawFd,
    opcode: u32,
    arg: *const std::ffi::c_void,
    nr_args: u32,
) -> std::io::Result<i32> {
    let ret = unsafe {
        raw_syscall(
            libc::SYS_io_uring_register,
            ring_fd as libc::c_ulong,
            opcode as libc::c_ulong,
            arg as libc::c_ulong,
            nr_args as libc::c_ulong,
            0,
            0,
        )
    };
    if ret < 0 {
        Err(std::io::Error::from_raw_os_error(-ret as i32))
    } else {
        Ok(ret as i32)
    }
}
