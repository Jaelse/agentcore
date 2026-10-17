//! Pseudo-terminals for agents: the live view's screen, and keyboard input
//! from people watching it.

use crate::{Result, SandboxError};

/// Non-blocking reader for a PTY master (`tokio::fs::File` would park a
/// blocking thread on every read).
pub(crate) struct PtyReader(tokio::io::unix::AsyncFd<std::fs::File>);

impl PtyReader {
    pub(crate) fn new(master: std::fs::File) -> Result<Self> {
        use nix::fcntl::{FcntlArg, OFlag, fcntl};
        let flags = fcntl(&master, FcntlArg::F_GETFL)
            .map_err(|e| SandboxError::Command(format!("pseudo-terminal: {e}")))?;
        fcntl(
            &master,
            FcntlArg::F_SETFL(OFlag::from_bits_truncate(flags) | OFlag::O_NONBLOCK),
        )
        .map_err(|e| SandboxError::Command(format!("pseudo-terminal: {e}")))?;
        tokio::io::unix::AsyncFd::new(master)
            .map(Self)
            .map_err(SandboxError::io("register pseudo-terminal"))
    }
}

impl tokio::io::AsyncRead for PtyReader {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        use std::io::Read;
        use std::task::Poll;
        loop {
            let mut guard = match self.0.poll_read_ready(cx) {
                Poll::Ready(Ok(guard)) => guard,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };
            let unfilled = buf.initialize_unfilled();
            match guard.try_io(|fd| fd.get_ref().read(unfilled)) {
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                // EIO: every process holding the terminal has exited.
                Ok(Err(e)) if e.raw_os_error() == Some(nix::libc::EIO) => {
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                Err(_would_block) => continue,
            }
        }
    }
}

/// A pseudo-terminal of the live view's size: (master, slave).
///
/// Both ends are opened close-on-exec, so a process spawned at the same time
/// for another session cannot inherit them: a stray copy of the slave would
/// keep the terminal open after the agent exits, and its last output would
/// never be read.
pub(crate) fn open_pty() -> Result<(std::fs::File, std::os::fd::OwnedFd)> {
    use agentcore_core::live::{TERMINAL_COLS, TERMINAL_ROWS};
    use rustix::fs::{Mode, OFlags};
    use rustix::pty::{OpenptFlags, grantpt, openpt, ptsname, unlockpt};
    let err =
        |e: rustix::io::Errno| SandboxError::Command(format!("cannot open a pseudo-terminal: {e}"));
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let master =
        openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY | OpenptFlags::CLOEXEC).map_err(err)?;
    // Elsewhere (macOS: development only) `posix_openpt` takes no
    // close-on-exec flag; set it right after.
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let master = {
        let master = openpt(OpenptFlags::RDWR | OpenptFlags::NOCTTY).map_err(err)?;
        rustix::io::fcntl_setfd(&master, rustix::io::FdFlags::CLOEXEC).map_err(err)?;
        master
    };
    grantpt(&master).map_err(err)?;
    unlockpt(&master).map_err(err)?;
    let name = ptsname(&master, Vec::new()).map_err(err)?;
    let slave = rustix::fs::open(
        name.as_c_str(),
        OFlags::RDWR | OFlags::NOCTTY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(err)?;
    let size = rustix::termios::Winsize {
        ws_row: TERMINAL_ROWS,
        ws_col: TERMINAL_COLS,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    rustix::termios::tcsetwinsize(&slave, size).map_err(err)?;
    Ok((std::fs::File::from(master), slave))
}

/// Non-blocking writer for a PTY master: what people type into the live view.
pub(crate) struct PtyWriter(tokio::io::unix::AsyncFd<std::fs::File>);

impl PtyWriter {
    /// A writer on a duplicate of `master` (which [`PtyReader::new`] has made
    /// non-blocking; the flag is shared by both descriptors).
    pub(crate) fn new(master: &std::fs::File) -> Result<Self> {
        let dup = master
            .try_clone()
            .map_err(SandboxError::io("duplicate pseudo-terminal"))?;
        tokio::io::unix::AsyncFd::new(dup)
            .map(Self)
            .map_err(SandboxError::io("register pseudo-terminal"))
    }
}

impl tokio::io::AsyncWrite for PtyWriter {
    fn poll_write(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        use std::io::Write;
        use std::task::Poll;
        loop {
            let mut guard = match self.0.poll_write_ready(cx) {
                Poll::Ready(Ok(guard)) => guard,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => return Poll::Pending,
            };
            match guard.try_io(|fd| fd.get_ref().write(buf)) {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }

    fn poll_flush(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }

    fn poll_shutdown(
        self: std::pin::Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        std::task::Poll::Ready(Ok(()))
    }
}
