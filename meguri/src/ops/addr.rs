//! Socket addresses in the layout the kernel reads and writes.

use std::mem;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV4, SocketAddrV6};

use crate::error::{MeguriError, MeguriResult};

/// A `sockaddr_storage` and its length, boxed so its address stays put while
/// the kernel uses it.
pub(super) struct RawAddr {
    pub(super) storage: libc::sockaddr_storage,
    pub(super) len: libc::socklen_t,
}

impl RawAddr {
    /// Empty storage for the kernel to fill (`accept`).
    pub(super) fn empty() -> Box<Self> {
        Box::new(Self {
            // SAFETY: all-zero is a valid `sockaddr_storage`.
            storage: unsafe { mem::zeroed() },
            len: mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t,
        })
    }

    /// Encode a std address (`connect`).
    pub(super) fn from_std(addr: SocketAddr) -> Box<Self> {
        let mut raw = Self::empty();
        match addr {
            SocketAddr::V4(v4) => {
                let sin = libc::sockaddr_in {
                    sin_family: libc::AF_INET as libc::sa_family_t,
                    sin_port: v4.port().to_be(),
                    sin_addr: libc::in_addr {
                        s_addr: u32::from(*v4.ip()).to_be(),
                    },
                    sin_zero: [0; 8],
                };
                // SAFETY: sockaddr_storage is large and aligned enough for
                // any sockaddr_* type.
                unsafe { std::ptr::write((&raw mut raw.storage).cast(), sin) };
                raw.len = mem::size_of::<libc::sockaddr_in>() as libc::socklen_t;
            }
            SocketAddr::V6(v6) => {
                let sin6 = libc::sockaddr_in6 {
                    sin6_family: libc::AF_INET6 as libc::sa_family_t,
                    sin6_port: v6.port().to_be(),
                    sin6_flowinfo: v6.flowinfo(),
                    sin6_addr: libc::in6_addr {
                        s6_addr: v6.ip().octets(),
                    },
                    sin6_scope_id: v6.scope_id(),
                };
                // SAFETY: as above.
                unsafe { std::ptr::write((&raw mut raw.storage).cast(), sin6) };
                raw.len = mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t;
            }
        }
        raw
    }

    /// Decode what the kernel wrote. IPv4 and IPv6 only.
    pub(super) fn to_std(&self) -> MeguriResult<SocketAddr> {
        match i32::from(self.storage.ss_family) {
            libc::AF_INET => {
                // SAFETY: the family says the storage holds a sockaddr_in.
                let sin: &libc::sockaddr_in = unsafe { &*(&raw const self.storage).cast() };
                Ok(SocketAddr::V4(SocketAddrV4::new(
                    Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)),
                    u16::from_be(sin.sin_port),
                )))
            }
            libc::AF_INET6 => {
                // SAFETY: the family says the storage holds a sockaddr_in6.
                let sin6: &libc::sockaddr_in6 = unsafe { &*(&raw const self.storage).cast() };
                Ok(SocketAddr::V6(SocketAddrV6::new(
                    Ipv6Addr::from(sin6.sin6_addr.s6_addr),
                    u16::from_be(sin6.sin6_port),
                    sin6.sin6_flowinfo,
                    sin6.sin6_scope_id,
                )))
            }
            _ => Err(MeguriError::InvalidArgument(
                "unsupported address family (IPv4 and IPv6 only)",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_v4_and_v6() {
        for addr in ["127.0.0.1:8080", "[::1]:443", "[fe80::1%2]:9"] {
            let addr: SocketAddr = addr.parse().unwrap();
            assert_eq!(RawAddr::from_std(addr).to_std().unwrap(), addr);
        }
    }
}
