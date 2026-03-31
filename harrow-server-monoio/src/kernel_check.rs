//! Kernel version check for io_uring support.
//!
//! This crate requires Linux kernel 6.1+ for full io_uring feature support.
//! We fail fast at startup rather than attempting fallbacks.

/// Minimum required kernel version: 6.1.0
const MIN_KERNEL_MAJOR: u32 = 6;
const MIN_KERNEL_MINOR: u32 = 1;

/// Error returned when kernel version is insufficient.
#[derive(Debug)]
pub struct KernelVersionError {
    pub current: String,
    pub required: String,
}

impl std::fmt::Display for KernelVersionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "Kernel version {} is too old. Required: {}. harrow-server-monoio requires Linux 6.1+ for io_uring support.",
            self.current, self.required
        )
    }
}

impl std::error::Error for KernelVersionError {}

/// Check if the current kernel meets requirements.
pub fn check_kernel_version() -> Result<(), KernelVersionError> {
    let version = get_kernel_version()?;

    if version.major < MIN_KERNEL_MAJOR
        || (version.major == MIN_KERNEL_MAJOR && version.minor < MIN_KERNEL_MINOR)
    {
        return Err(KernelVersionError {
            current: format!("{}.{}", version.major, version.minor),
            required: format!("{}.{}", MIN_KERNEL_MAJOR, MIN_KERNEL_MINOR),
        });
    }

    Ok(())
}

/// Kernel version parsed from uname.
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
struct KernelVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

fn get_kernel_version() -> Result<KernelVersion, KernelVersionError> {
    // Try to read from /proc/sys/kernel/osrelease first
    if let Ok(version_str) = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        && let Ok(version) = parse_version(&version_str)
    {
        return Ok(version);
    }

    // Fallback to uname syscall
    unsafe {
        let mut utsname: libc::utsname = std::mem::zeroed();
        if libc::uname(&mut utsname) == 0 {
            let release = std::ffi::CStr::from_ptr(utsname.release.as_ptr()).to_string_lossy();
            if let Ok(version) = parse_version(&release) {
                return Ok(version);
            }
        }
    }

    Err(KernelVersionError {
        current: "unknown".to_string(),
        required: format!("{}.{}", MIN_KERNEL_MAJOR, MIN_KERNEL_MINOR),
    })
}

fn parse_version(version_str: &str) -> Result<KernelVersion, ()> {
    // Parse version string like "6.1.0-foo" or "5.15.0-generic"
    let parts: Vec<&str> = version_str.trim().split('.').collect();
    if parts.len() >= 2 {
        let major = parts[0].parse().map_err(|_| ())?;
        let minor = parts[1].parse().map_err(|_| ())?;
        let patch = parts
            .get(2)
            .and_then(|s| s.split('-').next()?.parse().ok())
            .unwrap_or(0);

        return Ok(KernelVersion {
            major,
            minor,
            patch,
        });
    }

    Err(())
}

/// Probe whether io_uring is actually usable in this environment.
///
/// Even on kernels >= 6.1, Docker's default seccomp profile blocks io_uring
/// syscalls. This function attempts a minimal `io_uring_setup` syscall to
/// detect whether io_uring is available or the runtime fell back to epoll.
#[cfg(target_os = "linux")]
pub fn detect_io_driver() -> IoDriver {
    const SYS_IO_URING_SETUP: libc::c_long = 425;

    // io_uring_params is 120 bytes on both aarch64 and x86_64
    let mut params = [0u8; 120];
    unsafe {
        let fd = libc::syscall(SYS_IO_URING_SETUP, 1u32, params.as_mut_ptr());
        if fd >= 0 {
            libc::close(fd as i32);
            IoDriver::IoUring
        } else {
            IoDriver::Epoll
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub fn detect_io_driver() -> IoDriver {
    IoDriver::Epoll
}

/// The I/O driver in use by the monoio runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoDriver {
    /// io_uring is available and will be used.
    IoUring,
    /// io_uring is blocked (seccomp/kernel); monoio falls back to epoll.
    Epoll,
}

impl std::fmt::Display for IoDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IoDriver::IoUring => write!(f, "io_uring"),
            IoDriver::Epoll => write!(f, "epoll (io_uring unavailable)"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_version() {
        let v = parse_version("6.1.0").unwrap();
        assert_eq!(v.major, 6);
        assert_eq!(v.minor, 1);
        assert_eq!(v.patch, 0);

        let v = parse_version("6.1.0-ubuntu").unwrap();
        assert_eq!(v.major, 6);
        assert_eq!(v.minor, 1);
        assert_eq!(v.patch, 0);

        let v = parse_version("5.15.0-generic").unwrap();
        assert_eq!(v.major, 5);
        assert_eq!(v.minor, 15);
        assert_eq!(v.patch, 0);

        let v = parse_version("6.8.0-31-generic").unwrap();
        assert_eq!(v.major, 6);
        assert_eq!(v.minor, 8);
        assert_eq!(v.patch, 0);
    }

    #[test]
    fn test_version_check() {
        // These would need mocking to test properly
        // Just verify the parsing logic works
        assert!(parse_version("6.1.0").is_ok());
        assert!(parse_version("6.0.0").is_ok());
        assert!(parse_version("5.15.0").is_ok());
    }
}
