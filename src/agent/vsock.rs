//! Minimal vsock socket implementation using libc.
//! AF_VSOCK allows communication between the guest VM and the host
//! without any network configuration.

use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, RawFd};

// AF_VSOCK constants
const AF_VSOCK: i32 = 40;
const VMADDR_CID_ANY: u32 = 0xFFFFFFFF;

#[repr(C)]
struct SockaddrVm {
    svm_family: u16,
    svm_reserved1: u16,
    svm_port: u32,
    svm_cid: u32,
    svm_zero: [u8; 4],
}

pub struct VsockListener {
    fd: RawFd,
}

impl VsockListener {
    pub fn bind(port: u32) -> io::Result<Self> {
        unsafe {
            let fd = libc::socket(AF_VSOCK, libc::SOCK_STREAM, 0);
            if fd < 0 {
                return Err(io::Error::last_os_error());
            }

            let addr = SockaddrVm {
                svm_family: AF_VSOCK as u16,
                svm_reserved1: 0,
                svm_port: port,
                svm_cid: VMADDR_CID_ANY,
                svm_zero: [0; 4],
            };

            let ret = libc::bind(
                fd,
                &addr as *const SockaddrVm as *const libc::sockaddr,
                std::mem::size_of::<SockaddrVm>() as u32,
            );
            if ret < 0 {
                libc::close(fd);
                return Err(io::Error::last_os_error());
            }

            let ret = libc::listen(fd, 128);
            if ret < 0 {
                libc::close(fd);
                return Err(io::Error::last_os_error());
            }

            Ok(VsockListener { fd })
        }
    }

    pub fn accept(&self) -> io::Result<VsockStream> {
        unsafe {
            let client_fd = libc::accept(self.fd, std::ptr::null_mut(), std::ptr::null_mut());
            if client_fd < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(VsockStream { fd: client_fd })
        }
    }
}

impl Drop for VsockListener {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}

pub struct VsockStream {
    fd: RawFd,
}

impl AsRawFd for VsockStream {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl Read for VsockStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        unsafe {
            let n = libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len());
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        }
    }
}

impl Write for VsockStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        unsafe {
            let n = libc::write(self.fd, buf.as_ptr() as *const libc::c_void, buf.len());
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// Implement Read/Write for &VsockStream (needed for BufReader)
impl Read for &VsockStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        unsafe {
            let n = libc::read(self.fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len());
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        }
    }
}

impl Write for &VsockStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        unsafe {
            let n = libc::write(self.fd, buf.as_ptr() as *const libc::c_void, buf.len());
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for VsockStream {
    fn drop(&mut self) {
        unsafe {
            libc::close(self.fd);
        }
    }
}
