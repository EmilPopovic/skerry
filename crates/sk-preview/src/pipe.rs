// SPDX-License-Identifier: Apache-2.0
//! A deadline covers every byte of a helper exchange, including partial frames.

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

pub(crate) struct Timed<T> {
    inner: T,
    cancel: sk_util::Cancel,
    deadline: Instant,
    setup_error: Option<i32>,
}

impl<T: AsRawFd> Timed<T> {
    pub(crate) fn new(inner: T, cancel: sk_util::Cancel, limit: Duration) -> Self {
        // SAFETY: `inner` owns this pipe throughout these descriptor operations.
        let setup_error = unsafe {
            let flags = libc::fcntl(inner.as_raw_fd(), libc::F_GETFL);
            if flags < 0
                || libc::fcntl(inner.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) < 0
            {
                Some(io::Error::last_os_error().raw_os_error().unwrap_or(libc::EIO))
            } else {
                None
            }
        };
        Self { inner, cancel, deadline: Instant::now() + limit, setup_error }
    }

    pub(crate) fn until(&mut self, deadline: Instant) {
        self.deadline = deadline;
    }

    pub(crate) fn into_inner(self) -> T {
        self.inner
    }

    fn ready(&self, events: i16) -> io::Result<()> {
        if let Some(error) = self.setup_error {
            return Err(io::Error::from_raw_os_error(error));
        }
        loop {
            if self.cancel.is_cancelled() {
                return Err(io::Error::new(io::ErrorKind::ConnectionAborted, "helper cancelled"));
            }
            let left = self.deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(io::Error::new(io::ErrorKind::TimedOut, "helper deadline"));
            }
            let mut fd = libc::pollfd { fd: self.inner.as_raw_fd(), events, revents: 0 };
            let millis = i32::try_from(left.as_millis().clamp(1, 100)).unwrap_or(100);
            // SAFETY: one initialized poll descriptor remains alive for the call.
            let result = unsafe { libc::poll(&raw mut fd, 1, millis) };
            if result > 0 {
                return Ok(());
            }
            if result < 0 {
                let error = io::Error::last_os_error();
                if error.kind() != io::ErrorKind::Interrupted {
                    return Err(error);
                }
            }
        }
    }
}

impl<T: Read + AsRawFd> Read for Timed<T> {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        loop {
            self.ready(libc::POLLIN)?;
            match self.inner.read(bytes) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                result => return result,
            }
        }
    }
}

impl<T: Write + AsRawFd> Write for Timed<T> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        loop {
            self.ready(libc::POLLOUT)?;
            match self.inner.write(bytes) {
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                result => return result,
            }
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    fn pair() -> (std::fs::File, std::fs::File) {
        let (read, write) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).unwrap();
        (read.into(), write.into())
    }

    #[test]
    fn partial_header_and_silent_peer_have_a_deadline() {
        for prefix in [b"".as_slice(), b"ab".as_slice()] {
            let (reader, mut peer) = pair();
            peer.write_all(prefix).unwrap();
            let mut timed = Timed::new(reader, sk_util::Cancel::new(), Duration::from_millis(25));
            let started = Instant::now();
            assert_eq!(timed.read_exact(&mut [0; 4]).unwrap_err().kind(), io::ErrorKind::TimedOut);
            assert!(started.elapsed() < Duration::from_secs(1));
        }
    }

    #[test]
    fn an_unread_request_pipe_times_out_and_cancellation_is_not_retried() {
        let (_peer, writer) = pair();
        let cancel = sk_util::Cancel::new();
        let mut timed = Timed::new(writer, cancel.clone(), Duration::from_millis(25));
        assert_eq!(timed.write_all(&vec![0; 4 << 20]).unwrap_err().kind(), io::ErrorKind::TimedOut);
        cancel.cancel();
        assert_eq!(
            timed.read_exact(&mut [0; 4]).unwrap_err().kind(),
            io::ErrorKind::ConnectionAborted
        );
    }
}
