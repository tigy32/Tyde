use super::GroupCommand;
use portable_pty::{Child, MasterPty, PtySize};

pub struct Session {
    pub master: Box<dyn MasterPty + Send>,
    pub child: Box<dyn Child + Send + Sync>,
}

#[cfg(unix)]
pub fn spawn(mut command: GroupCommand, size: PtySize) -> std::io::Result<Session> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let mut master = -1;
    let mut slave = -1;
    #[cfg(target_os = "linux")]
    let size = unix::winsize(size);
    #[cfg(not(target_os = "linux"))]
    let mut size = unix::winsize(size);
    #[cfg(target_os = "linux")]
    let dimensions = &size;
    #[cfg(not(target_os = "linux"))]
    let dimensions = &mut size;
    if unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            dimensions,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let master = unsafe { std::fs::File::from_raw_fd(master) };
    let slave = unsafe { std::fs::File::from_raw_fd(slave) };
    for file in [&master, &slave] {
        let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) };
        if flags < 0
            || unsafe { libc::fcntl(file.as_raw_fd(), libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    command.session = super::SessionMode::Terminal;
    command
        .stdin(slave.try_clone()?)
        .stdout(slave.try_clone()?)
        .stderr(slave);
    let child = command.spawn_blocking()?;
    Ok(Session {
        master: Box::new(unix::Master {
            file: master,
            writer_taken: std::cell::Cell::new(false),
        }),
        child: Box::new(unix::Process(child)),
    })
}

#[cfg(windows)]
pub fn spawn(command: GroupCommand, size: PtySize) -> std::io::Result<Session> {
    let pair = portable_pty::native_pty_system()
        .openpty(size)
        .map_err(std::io::Error::other)?;
    let mut builder = portable_pty::CommandBuilder::new(command.program);
    builder.args(command.args);
    for (key, value) in command.env {
        match value {
            Some(value) => {
                builder.env(key, value);
            }
            None => {
                builder.env_remove(key);
            }
        }
    }
    if let Some(cwd) = command.cwd {
        builder.cwd(cwd);
    }
    let child = pair
        .slave
        .spawn_command(builder)
        .map_err(std::io::Error::other)?;
    Ok(Session {
        master: pair.master,
        child,
    })
}

#[cfg(unix)]
mod unix {
    use super::*;
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    pub(super) fn winsize(size: PtySize) -> libc::winsize {
        libc::winsize {
            ws_row: size.rows,
            ws_col: size.cols,
            ws_xpixel: size.pixel_width,
            ws_ypixel: size.pixel_height,
        }
    }
    pub(super) struct Master {
        pub file: std::fs::File,
        pub writer_taken: std::cell::Cell<bool>,
    }
    impl MasterPty for Master {
        fn resize(&self, size: PtySize) -> anyhow::Result<()> {
            if unsafe { libc::ioctl(self.file.as_raw_fd(), libc::TIOCSWINSZ, &winsize(size)) } != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            Ok(())
        }
        fn get_size(&self) -> anyhow::Result<PtySize> {
            let mut size = std::mem::MaybeUninit::<libc::winsize>::zeroed();
            if unsafe { libc::ioctl(self.file.as_raw_fd(), libc::TIOCGWINSZ, size.as_mut_ptr()) }
                != 0
            {
                return Err(std::io::Error::last_os_error().into());
            }
            let size = unsafe { size.assume_init() };
            Ok(PtySize {
                rows: size.ws_row,
                cols: size.ws_col,
                pixel_width: size.ws_xpixel,
                pixel_height: size.ws_ypixel,
            })
        }
        fn try_clone_reader(&self) -> anyhow::Result<Box<dyn Read + Send>> {
            Ok(Box::new(Reader(self.file.try_clone()?)))
        }
        fn take_writer(&self) -> anyhow::Result<Box<dyn Write + Send>> {
            if self.writer_taken.replace(true) {
                return Err(std::io::Error::other("terminal writer already acquired").into());
            }
            Ok(Box::new(Writer(self.file.try_clone()?)))
        }
        fn get_termios(&self) -> Option<nix::sys::termios::Termios> {
            nix::sys::termios::tcgetattr(self.file.as_raw_fd()).ok()
        }
        fn as_raw_fd(&self) -> Option<libc::c_int> {
            Some(self.file.as_raw_fd())
        }
        fn process_group_leader(&self) -> Option<libc::pid_t> {
            let pid = unsafe { libc::tcgetpgrp(self.file.as_raw_fd()) };
            (pid > 0).then_some(pid)
        }
    }
    struct Reader(std::fs::File);
    impl Read for Reader {
        fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
            match self.0.read(buffer) {
                Err(error) if error.raw_os_error() == Some(libc::EIO) => Ok(0),
                result => result,
            }
        }
    }
    struct Writer(std::fs::File);
    impl Write for Writer {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.write(buffer)
        }
        fn flush(&mut self) -> std::io::Result<()> {
            self.0.flush()
        }
    }
    impl Drop for Writer {
        fn drop(&mut self) {
            let mut attributes = std::mem::MaybeUninit::<libc::termios>::zeroed();
            if unsafe { libc::tcgetattr(self.0.as_raw_fd(), attributes.as_mut_ptr()) } == 0 {
                let eof = unsafe { attributes.assume_init() }.c_cc[libc::VEOF];
                if eof != 0
                    && let Err(error) = self.0.write_all(&[b'\n', eof])
                {
                    tracing::debug!(error_kind=?error.kind(), "terminal EOF delivery ended");
                }
            }
        }
    }
    pub(super) struct Process(pub super::super::BlockingChild);
    impl std::fmt::Debug for Process {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("TerminalProcess")
                .finish_non_exhaustive()
        }
    }
    struct Signaller(super::super::SharedProcess);
    impl std::fmt::Debug for Signaller {
        fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter
                .debug_struct("TerminalSignaller")
                .finish_non_exhaustive()
        }
    }
    impl portable_pty::ChildKiller for Signaller {
        fn kill(&mut self) -> std::io::Result<()> {
            self.0.lock().expect("process signal mutex poisoned").kill()
        }
        fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
            Box::new(Self(std::sync::Arc::clone(&self.0)))
        }
    }
    impl portable_pty::ChildKiller for Process {
        fn kill(&mut self) -> std::io::Result<()> {
            self.0.kill()
        }
        fn clone_killer(&self) -> Box<dyn portable_pty::ChildKiller + Send + Sync> {
            Box::new(Signaller(std::sync::Arc::clone(&self.0.child.process)))
        }
    }
    impl portable_pty::Child for Process {
        fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
            self.0.try_wait().map(|status| status.map(Into::into))
        }
        fn wait(&mut self) -> std::io::Result<portable_pty::ExitStatus> {
            self.0.wait().map(Into::into)
        }
        fn process_id(&self) -> Option<u32> {
            self.0.id()
        }
    }
}
