// Copyright © 2021 Intel Corporation
//
// SPDX-License-Identifier: Apache-2.0
//

use std::io::{self, ErrorKind, Read, Write};
use std::net::Shutdown;
use std::os::fd::{AsFd, AsRawFd, OwnedFd, RawFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::SerialBuffer;

struct SharedSerialBuffer(Arc<Mutex<SerialBuffer>>);

impl Write for SharedSerialBuffer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap().flush()
    }
}

/// Serves a device byte stream over a socket to one client at a time, buffering
/// output until a client connects.
pub struct SocketConsole {
    buffer: Arc<Mutex<SerialBuffer>>,
    write_out: Arc<AtomicBool>,
    reader: Option<UnixStream>,
    writable: Option<OwnedFd>,
}

impl Default for SocketConsole {
    fn default() -> Self {
        Self::new()
    }
}

impl SocketConsole {
    pub fn new() -> Self {
        let write_out = Arc::new(AtomicBool::new(false));
        let buffer = Arc::new(Mutex::new(SerialBuffer::new(
            Box::new(io::sink()),
            write_out.clone(),
        )));
        Self {
            buffer,
            write_out,
            reader: None,
            writable: None,
        }
    }

    pub fn out_sink(&self) -> Box<dyn Write + Send> {
        Box::new(SharedSerialBuffer(self.buffer.clone()))
    }

    /// Returns the connected client fd, or None when no client is attached.
    pub fn client_fd(&self) -> Option<RawFd> {
        self.reader.as_ref().map(|stream| stream.as_raw_fd())
    }

    /// Returns a duplicate of the client fd to poll for writability, or None
    /// when no client is attached. Output the socket could not take stays
    /// buffered until the next flush, so flush when this fd becomes writable.
    /// Being a separate fd, it can carry its own epoll event mask alongside
    /// client_fd().
    pub fn writable_fd(&self) -> Option<RawFd> {
        self.writable.as_ref().map(|fd| fd.as_raw_fd())
    }

    pub fn shutdown(&mut self) -> io::Result<()> {
        if let Some(stream) = self.reader.take() {
            self.writable = None;
            let res = stream.shutdown(Shutdown::Both);
            self.detach();
            res?;
        }
        Ok(())
    }

    pub fn accept(&mut self, listener: &UnixListener) -> io::Result<()> {
        let (stream, _) = listener.accept()?;
        stream.set_nonblocking(true)?;
        let writer = stream.try_clone()?;
        let writable = stream.as_fd().try_clone_to_owned()?;
        self.attach(Box::new(writer))?;
        self.reader = Some(stream);
        self.writable = Some(writable);
        Ok(())
    }

    fn attach(&self, writer: Box<dyn Write + Send>) -> io::Result<()> {
        let mut buffer = self.buffer.lock().unwrap();
        buffer.set_out(writer);
        self.write_out.store(true, Ordering::Release);
        buffer.flush()
    }

    fn detach(&self) {
        self.write_out.store(false, Ordering::Release);
        self.buffer.lock().unwrap().set_out(Box::new(io::sink()));
    }
}

impl Read for SocketConsole {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let Some(mut stream) = self.reader.as_ref() else {
            return Ok(0);
        };
        match stream.read(buf) {
            Ok(count) if count > 0 => Ok(count),
            Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(0),
            ended => {
                let _ = stream.shutdown(Shutdown::Both);
                self.reader = None;
                self.writable = None;
                self.detach();
                ended
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::{ErrorKind, Read, Write};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::thread;
    use std::time::Duration;

    use vmm_sys_util::epoll::{ControlOperation, Epoll, EpollEvent, EventSet};
    use vmm_sys_util::tempdir::TempDir;

    use super::SocketConsole;

    fn read_retry(console: &mut SocketConsole, buf: &mut [u8]) -> usize {
        for _ in 0..200 {
            let n = console.read(buf).unwrap();
            if n > 0 {
                return n;
            }
            thread::sleep(Duration::from_millis(1));
        }
        0
    }

    #[test]
    fn replays_buffered_output_to_client_on_connect() {
        let tmp_dir = TempDir::new().unwrap();
        let path = tmp_dir.as_path().join("socket");
        let listener = UnixListener::bind(&path).unwrap();
        let mut console = SocketConsole::new();

        let mut sink = console.out_sink();
        sink.write_all(b"boot\n").unwrap();

        let mut client = UnixStream::connect(&path).unwrap();
        console.accept(&listener).unwrap();

        let mut got = [0u8; 5];
        client.read_exact(&mut got).unwrap();
        assert_eq!(&got, b"boot\n");
    }

    #[test]
    fn reads_input_from_connected_client() {
        let tmp_dir = TempDir::new().unwrap();
        let path = tmp_dir.as_path().join("socket");
        let listener = UnixListener::bind(&path).unwrap();
        let mut console = SocketConsole::new();

        let mut client = UnixStream::connect(&path).unwrap();
        console.accept(&listener).unwrap();

        client.write_all(b"hi").unwrap();
        client.flush().unwrap();

        let mut buf = [0u8; 8];
        let n = read_retry(&mut console, &mut buf);
        assert_eq!(&buf[..n], b"hi");
    }

    #[test]
    fn shutdown_then_accept_replaces_client() {
        let tmp_dir = TempDir::new().unwrap();
        let path = tmp_dir.as_path().join("socket");
        let listener = UnixListener::bind(&path).unwrap();
        let mut console = SocketConsole::new();

        let mut first = UnixStream::connect(&path).unwrap();
        console.accept(&listener).unwrap();

        let second = UnixStream::connect(&path).unwrap();
        console.shutdown().unwrap();
        console.accept(&listener).unwrap();

        // shutdown() closed the first connection.
        let mut discard = [0u8; 4];
        assert_eq!(first.read(&mut discard).unwrap(), 0);

        let mut sink = console.out_sink();
        sink.write_all(b"hey\n").unwrap();
        let mut got = [0u8; 4];
        (&second).read_exact(&mut got).unwrap();
        assert_eq!(&got, b"hey\n");
    }

    #[test]
    fn reports_connection_state() {
        let tmp_dir = TempDir::new().unwrap();
        let path = tmp_dir.as_path().join("socket");
        let listener = UnixListener::bind(&path).unwrap();
        let mut console = SocketConsole::new();
        assert!(console.client_fd().is_none());
        assert!(console.writable_fd().is_none());

        let client = UnixStream::connect(&path).unwrap();
        console.accept(&listener).unwrap();
        assert!(console.client_fd().is_some());
        assert!(console.writable_fd().is_some());

        // Hanging up drops both fds, so the socket is released.
        drop(client);
        assert_eq!(console.read(&mut [0u8; 1]).unwrap(), 0);
        assert!(console.client_fd().is_none());
        assert!(console.writable_fd().is_none());
    }

    // Output the socket cannot take is held in the buffer. Each time the
    // client frees space, the writable fd reports one edge, and a flush on it
    // sends held bytes without the device writing again.
    #[test]
    fn flushes_held_output_when_socket_becomes_writable() {
        const INPUT: u64 = 0;
        const WRITABLE: u64 = 1;

        let tmp_dir = TempDir::new().unwrap();
        let path = tmp_dir.as_path().join("socket");
        let listener = UnixListener::bind(&path).unwrap();
        let mut console = SocketConsole::new();

        let client = UnixStream::connect(&path).unwrap();
        client.set_nonblocking(true).unwrap();
        console.accept(&listener).unwrap();

        // Watch the client as the serial manager does: input level-triggered,
        // writability edge-triggered on the duplicate fd.
        let epoll = Epoll::new().unwrap();
        epoll
            .ctl(
                ControlOperation::Add,
                console.client_fd().unwrap(),
                EpollEvent::new(EventSet::IN, INPUT),
            )
            .unwrap();
        epoll
            .ctl(
                ControlOperation::Add,
                console.writable_fd().unwrap(),
                EpollEvent::new(EventSet::OUT | EventSet::EDGE_TRIGGERED, WRITABLE),
            )
            .unwrap();
        let mut events = [EpollEvent::default(); 2];
        let mut wait = |timeout| {
            let n = epoll.wait(timeout, &mut events).unwrap();
            events[..n].iter().map(EpollEvent::data).collect::<Vec<_>>()
        };

        // An idle socket is writable: one edge, and no repeat while it stays so.
        assert_eq!(wait(0), [WRITABLE]);
        assert!(wait(0).is_empty());

        // Write until the socket is full and output is held back. The pattern
        // period is prime to the chunk size, so reordered chunks cannot match.
        let mut sink = console.out_sink();
        let mut sent = Vec::new();
        while console.buffer.lock().unwrap().buffer.is_empty() {
            let chunk: Vec<u8> = (sent.len()..sent.len() + 64)
                .map(|i| (i % 251) as u8)
                .collect();
            sink.write_all(&chunk).unwrap();
            sent.extend(chunk);
        }
        assert!(wait(0).is_empty());

        // From here the device writes nothing: held bytes reach the client
        // only through flushes on the edges that its reads produce.
        let mut received = Vec::new();
        loop {
            let err = (&client).read_to_end(&mut received).unwrap_err();
            assert_eq!(err.kind(), ErrorKind::WouldBlock);
            if received.len() == sent.len() {
                break;
            }
            assert_eq!(wait(1000), [WRITABLE]);
            sink.flush().unwrap();
        }
        assert_eq!(received, sent);
    }
}
