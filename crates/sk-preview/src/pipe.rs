// SPDX-License-Identifier: Apache-2.0
//! A deadline covers every byte of a helper exchange, including partial frames.

use std::io::{self, Read, Write};
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

pub(crate) struct Timed<T> {
    inner: T,
    cancel: sk_util::Cancel,
    deadline: Instant,
    /// Where set, each read and write gets this long from when it starts, rather
    /// than all of them sharing [`Timed::until`]'s one deadline.
    stall: Option<Duration>,
    setup_error: Option<i32>,
}

impl<T: AsRawFd> Timed<T> {
    /// A pipe whose every byte, from now, is due within `limit`.
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
        Self { inner, cancel, deadline: Instant::now() + limit, stall: None, setup_error }
    }

    /// A pipe that may take as long as it likes in all, but never `limit`
    /// without a byte.
    ///
    /// **For a frame whose length is an archive's rather than the protocol's**,
    /// where a deadline on the whole is one a large enough archive misses and
    /// what tells a wedged peer from an honest one is that it stopped moving.
    pub(crate) fn stalling(inner: T, cancel: sk_util::Cancel, limit: Duration) -> Self {
        Self { stall: Some(limit), ..Self::new(inner, cancel, limit) }
    }

    pub(crate) fn until(&mut self, deadline: Instant) {
        self.deadline = deadline;
    }

    pub(crate) fn get_ref(&self) -> &T {
        &self.inner
    }

    pub(crate) fn into_inner(self) -> T {
        self.inner
    }

    fn restart(&mut self) {
        if let Some(stall) = self.stall {
            self.deadline = Instant::now() + stall;
        }
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
        self.restart();
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
        self.restart();
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
        let (read, write) = rustix::pipe::pipe().unwrap();
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

    /// **A stall limit is about silence, not length.** A peer that trickles one
    /// byte at a time is honest however long the whole takes, and one that stops
    /// is given up the stall after its last byte rather than never.
    #[test]
    fn a_stall_limit_is_reset_by_every_byte_and_fires_on_silence() {
        let (reader, mut peer) = pair();
        let stall = Duration::from_millis(400);
        let mut timed = Timed::stalling(reader, sk_util::Cancel::new(), stall);
        std::thread::spawn(move || {
            for byte in 0..16u8 {
                std::thread::sleep(Duration::from_millis(40)); // skerry-allow: no-blocking-ui — a slow peer
                peer.write_all(&[byte]).unwrap();
            }
            // Holds the pipe open and silent, which is the wedged peer.
            std::thread::sleep(Duration::from_secs(5)); // skerry-allow: no-blocking-ui — a wedged peer
        });
        let started = Instant::now();
        let mut sixteen = [0u8; 16];
        timed.read_exact(&mut sixteen).unwrap();
        assert_eq!(sixteen[15], 15);
        assert!(started.elapsed() > stall, "the whole should have outlasted one stall");

        let silent = Instant::now();
        assert_eq!(timed.read(&mut [0; 1]).unwrap_err().kind(), io::ErrorKind::TimedOut);
        assert!(silent.elapsed() < Duration::from_secs(2), "{:?}", silent.elapsed());
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
