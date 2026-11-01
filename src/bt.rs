//! Minimal Bluetooth RFCOMM client over raw Linux sockets.
//!
//! Rust's standard library has no `AF_BLUETOOTH` support, and pulling in a
//! BlueZ/D-Bus crate for one socket is a lot of dependency for very little, so
//! this module declares the handful of libc calls it needs directly.

use std::io;
use std::os::raw::{c_int, c_ulong};

const AF_BLUETOOTH: c_int = 31;
const SOCK_STREAM: c_int = 1;
const BTPROTO_RFCOMM: c_int = 3;

const F_GETFL: c_int = 3;
const F_SETFL: c_int = 4;
const O_NONBLOCK: c_int = 0o4000;

const SOL_SOCKET: c_int = 1;
const SO_ERROR: c_int = 4;
const EINPROGRESS: i32 = 115;

const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;
const POLLERR: i16 = 0x008;
const POLLHUP: i16 = 0x010;

/// `sizeof(struct sockaddr_rc)` — 2-byte family, 6-byte address, 1-byte channel,
/// padded to the struct's 2-byte alignment.
const SOCKADDR_RC_LEN: u32 = 10;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct SockAddrRc {
    family: u16,
    bdaddr: [u8; 6],
    channel: u8,
    _pad: u8,
}

#[repr(C)]
struct PollFd {
    fd: c_int,
    events: i16,
    revents: i16,
}

extern "C" {
    fn socket(domain: c_int, ty: c_int, protocol: c_int) -> c_int;
    fn connect(fd: c_int, addr: *const SockAddrRc, len: u32) -> c_int;
    fn close(fd: c_int) -> c_int;
    fn read(fd: c_int, buf: *mut u8, count: usize) -> isize;
    fn write(fd: c_int, buf: *const u8, count: usize) -> isize;
    fn fcntl(fd: c_int, cmd: c_int, ...) -> c_int;
    fn poll(fds: *mut PollFd, nfds: c_ulong, timeout: c_int) -> c_int;
    fn getsockopt(
        fd: c_int,
        level: c_int,
        optname: c_int,
        optval: *mut u8,
        optlen: *mut u32,
    ) -> c_int;
    #[cfg(test)]
    fn socketpair(domain: c_int, ty: c_int, protocol: c_int, sv: *mut c_int) -> c_int;
}

/// Parse a `AA:BB:CC:DD:EE:FF` address.
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let parts: Vec<&str> = s.split([':', '-']).collect();
    if parts.len() != 6 {
        return None;
    }
    let mut out = [0u8; 6];
    for (i, p) in parts.iter().enumerate() {
        out[i] = u8::from_str_radix(p, 16).ok()?;
    }
    Some(out)
}

pub fn format_mac(m: &[u8; 6]) -> String {
    m.iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn wait_fd(fd: c_int, events: i16, timeout_ms: c_int) -> io::Result<i16> {
    let mut pfd = PollFd {
        fd,
        events,
        revents: 0,
    };
    let rc = unsafe { poll(&mut pfd, 1, timeout_ms) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    if rc == 0 {
        return Err(io::Error::new(io::ErrorKind::TimedOut, "timed out"));
    }
    Ok(pfd.revents)
}

/// An open RFCOMM connection, closed on drop.
pub struct Rfcomm {
    fd: c_int,
}

impl Rfcomm {
    /// Connect to `mac` on `channel`, failing within `timeout_ms`.
    pub fn connect(mac: &[u8; 6], channel: u8, timeout_ms: c_int) -> io::Result<Self> {
        // bdaddr_t is stored little-endian: the human "00:1A:7D:DA:71:13" goes
        // on the wire as 13 71 da 7d 1a 00. Passing the obvious byte order
        // makes the kernel page a nonexistent device and fail with EHOSTDOWN.
        let mut bdaddr = *mac;
        bdaddr.reverse();

        let addr = SockAddrRc {
            family: AF_BLUETOOTH as u16,
            bdaddr,
            channel,
            _pad: 0,
        };

        let fd = unsafe { socket(AF_BLUETOOTH, SOCK_STREAM, BTPROTO_RFCOMM) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }

        // Non-blocking connect so a wrong channel cannot hang the program.
        let flags = unsafe { fcntl(fd, F_GETFL) };
        if flags < 0 {
            let e = io::Error::last_os_error();
            unsafe { close(fd) };
            return Err(e);
        }
        if unsafe { fcntl(fd, F_SETFL, flags | O_NONBLOCK) } < 0 {
            let e = io::Error::last_os_error();
            unsafe { close(fd) };
            return Err(e);
        }

        let rc = unsafe { connect(fd, &addr, SOCKADDR_RC_LEN) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(EINPROGRESS) {
                unsafe { close(fd) };
                return Err(err);
            }
            let revents = match wait_fd(fd, POLLOUT, timeout_ms) {
                Ok(r) => r,
                Err(e) => {
                    unsafe { close(fd) };
                    return Err(e);
                }
            };
            if revents & (POLLERR | POLLHUP) != 0 {
                // A definitive refusal; report the real errno below.
            }
            let mut so_error: c_int = 0;
            let mut len: u32 = std::mem::size_of::<c_int>() as u32;
            let got = unsafe {
                getsockopt(
                    fd,
                    SOL_SOCKET,
                    SO_ERROR,
                    &mut so_error as *mut c_int as *mut u8,
                    &mut len,
                )
            };
            if got < 0 {
                let e = io::Error::last_os_error();
                unsafe { close(fd) };
                return Err(e);
            }
            if so_error != 0 {
                unsafe { close(fd) };
                return Err(io::Error::from_raw_os_error(so_error));
            }
        }

        // Back to blocking mode for normal I/O.
        if unsafe { fcntl(fd, F_SETFL, flags) } < 0 {
            let e = io::Error::last_os_error();
            unsafe { close(fd) };
            return Err(e);
        }

        Ok(Rfcomm { fd })
    }

    pub fn send(&mut self, data: &[u8]) -> io::Result<()> {
        let mut off = 0usize;
        while off < data.len() {
            let n = unsafe { write(self.fd, data[off..].as_ptr(), data.len() - off) };
            if n < 0 {
                let e = io::Error::last_os_error();
                if e.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(e);
            }
            if n == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "connection closed",
                ));
            }
            off += n as usize;
        }
        Ok(())
    }

    /// Read whatever is available, waiting up to `timeout_ms`.
    ///
    /// Returns `Ok(None)` on timeout.
    pub fn recv(&mut self, buf: &mut [u8], timeout_ms: c_int) -> io::Result<Option<usize>> {
        match wait_fd(self.fd, POLLIN, timeout_ms) {
            Ok(_) => {}
            Err(e) if e.kind() == io::ErrorKind::TimedOut => return Ok(None),
            Err(e) => return Err(e),
        }
        let n = unsafe { read(self.fd, buf.as_mut_ptr(), buf.len()) };
        if n < 0 {
            let e = io::Error::last_os_error();
            if e.kind() == io::ErrorKind::Interrupted {
                return Ok(Some(0));
            }
            return Err(e);
        }
        if n == 0 {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "connection closed by device",
            ));
        }
        Ok(Some(n as usize))
    }

    #[cfg(test)]
    pub fn pair() -> io::Result<(Self, Self)> {
        let mut fds = [0 as c_int; 2];
        // AF_UNIX = 1, SOCK_STREAM = 1
        let rc = unsafe { socketpair(1, SOCK_STREAM, 0, fds.as_mut_ptr()) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok((Rfcomm { fd: fds[0] }, Rfcomm { fd: fds[1] }))
    }
}

impl Drop for Rfcomm {
    fn drop(&mut self) {
        unsafe {
            close(self.fd);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_parsing() {
        let m = parse_mac("00:1A:7D:DA:71:13").unwrap();
        assert_eq!(m, [0x00, 0x1A, 0x7D, 0xDA, 0x71, 0x13]);
        assert_eq!(format_mac(&m), "00:1A:7D:DA:71:13");
        assert!(parse_mac("nonsense").is_none());
        assert!(parse_mac("00:1A:7D:DA:71").is_none());
    }

    #[test]
    fn sockaddr_layout_is_ten_bytes() {
        assert_eq!(std::mem::size_of::<SockAddrRc>() as u32, SOCKADDR_RC_LEN);
    }

    #[test]
    fn bdaddr_is_little_endian() {
        // What actually goes on the wire, per strace of a working connect.
        let human = parse_mac("00:1A:7D:DA:71:13").unwrap();
        let mut wire = human;
        wire.reverse();
        assert_eq!(wire, [0x13, 0x71, 0xDA, 0x7D, 0x1A, 0x00]);
    }

    #[test]
    fn rfcomm_pair_communication() {
        let (mut a, mut b) = Rfcomm::pair().unwrap();
        a.send(b"hello world").unwrap();
        let mut buf = [0u8; 32];
        let n = b.recv(&mut buf, 1000).unwrap().unwrap();
        assert_eq!(&buf[..n], b"hello world");
    }
}
