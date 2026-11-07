//! Minimal PTY support: allocate a pair, size it, and spawn a process with
//! the slave as its controlling terminal.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};

pub struct Pty {
    pub master: OwnedFd,
    pub slave: OwnedFd,
}

fn winsize(cols: u16, rows: u16) -> libc::winsize {
    libc::winsize { ws_row: rows, ws_col: cols, ws_xpixel: 0, ws_ypixel: 0 }
}

fn set_cloexec(fd: RawFd) -> io::Result<()> {
    // SAFETY: fcntl on a descriptor we own.
    let r = unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Open a PTY pair with the given size. Both ends are close-on-exec; the
/// slave is re-opened as fds 0-2 in the child by [`spawn`].
pub fn open(cols: u16, rows: u16) -> io::Result<Pty> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let mut ws = winsize(cols, rows);
    // SAFETY: openpty writes two descriptors into the provided ints.
    let r = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut ws,
        )
    };
    if r < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openpty succeeded, so both descriptors are valid and ours.
    let pty = unsafe {
        Pty { master: OwnedFd::from_raw_fd(master), slave: OwnedFd::from_raw_fd(slave) }
    };
    set_cloexec(pty.master.as_raw_fd())?;
    set_cloexec(pty.slave.as_raw_fd())?;
    Ok(pty)
}

/// Resize the terminal attached to `master`. The kernel sends SIGWINCH to
/// the foreground process group.
pub fn resize(master: RawFd, cols: u16, rows: u16) -> io::Result<()> {
    let ws = winsize(cols, rows);
    // SAFETY: TIOCSWINSZ reads a winsize struct from the pointer.
    let r = unsafe { libc::ioctl(master, libc::TIOCSWINSZ as _, &ws) };
    if r < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Spawn `cmd` in a new session with `slave` as its controlling terminal and
/// stdin/stdout/stderr. The child's pid is also its process-group id.
pub fn spawn(mut cmd: Command, slave: OwnedFd) -> io::Result<Child> {
    cmd.stdin(Stdio::from(slave.try_clone()?))
        .stdout(Stdio::from(slave.try_clone()?))
        .stderr(Stdio::from(slave));
    // SAFETY: only async-signal-safe calls between fork and exec.
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    // `cmd` (and the slave copies it holds) is dropped when this returns,
    // so the parent keeps no slave descriptor and reads on the master see
    // EOF/EIO once the child side is gone.
    cmd.spawn()
}
