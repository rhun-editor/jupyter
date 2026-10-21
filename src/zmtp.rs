//! Just enough ZeroMQ (ZMTP 3.0 over TCP, the NULL mechanism) to talk to a Jupyter kernel: a
//! DEALER for the shell and control channels and a SUB for iopub. The kernel's sockets bind on
//! the loopback address; these connect.

use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::Duration;

/// A connected peer: whole messages (lists of frames) in and out.
pub struct Socket {
    stream: TcpStream,
}

fn bad(what: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, what.to_string())
}

impl Socket {
    /// Connects and does the handshake as a `DEALER` or a `SUB` (which then subscribes to every
    /// message).
    pub fn connect(port: u16, kind: &str) -> io::Result<Socket> {
        let stream = TcpStream::connect(("127.0.0.1", port))?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut s = Socket { stream };
        let mut greeting = [0u8; 64];
        greeting[0] = 0xff;
        greeting[9] = 0x7f;
        greeting[10] = 3; // version 3.0: a SUB subscribes with a message, which 3.1 peers accept
        greeting[11] = 0;
        greeting[12..16].copy_from_slice(b"NULL");
        s.stream.write_all(&greeting)?;
        let mut peer = [0u8; 64];
        s.stream.read_exact(&mut peer)?;
        if peer[0] != 0xff || peer[9] != 0x7f || peer[10] < 3 || &peer[12..16] != b"NULL" {
            return Err(bad("the kernel's port does not speak ZMTP 3 with the NULL mechanism"));
        }
        let mut ready = Vec::new();
        ready.push(5u8);
        ready.extend_from_slice(b"READY");
        ready.push(11);
        ready.extend_from_slice(b"Socket-Type");
        ready.extend_from_slice(&(kind.len() as u32).to_be_bytes());
        ready.extend_from_slice(kind.as_bytes());
        s.write_frame(&ready, false, true)?;
        let (cmd, _) = s.read_frame()?;
        if !cmd.starts_with(b"\x05READY") {
            return Err(bad("the kernel did not answer READY"));
        }
        if kind == "SUB" {
            s.send(&[b"\x01".to_vec()])?;
        }
        s.stream.set_read_timeout(None)?;
        Ok(s)
    }

    pub fn try_clone(&self) -> io::Result<Socket> {
        Ok(Socket { stream: self.stream.try_clone()? })
    }

    pub fn close(&self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }

    fn write_frame(&mut self, data: &[u8], more: bool, command: bool) -> io::Result<()> {
        let mut flags = 0u8;
        if more {
            flags |= 1;
        }
        if command {
            flags |= 4;
        }
        let mut head = Vec::with_capacity(9 + data.len());
        if data.len() > 255 {
            head.push(flags | 2);
            head.extend_from_slice(&(data.len() as u64).to_be_bytes());
        } else {
            head.push(flags);
            head.push(data.len() as u8);
        }
        head.extend_from_slice(data);
        self.stream.write_all(&head)
    }

    /// One frame: its bytes and whether more of the message follows. Commands (PING and the
    /// like) come back with their command flag, as frames that are not part of a message.
    fn read_frame(&mut self) -> io::Result<(Vec<u8>, u8)> {
        let mut h = [0u8; 1];
        self.stream.read_exact(&mut h)?;
        let len = if h[0] & 2 != 0 {
            let mut b = [0u8; 8];
            self.stream.read_exact(&mut b)?;
            u64::from_be_bytes(b)
        } else {
            let mut b = [0u8; 1];
            self.stream.read_exact(&mut b)?;
            b[0] as u64
        };
        if len > 1 << 30 {
            return Err(bad("a frame over 1 GB"));
        }
        let mut data = vec![0u8; len as usize];
        self.stream.read_exact(&mut data)?;
        Ok((data, h[0]))
    }

    /// Sends a message as one write.
    pub fn send(&mut self, frames: &[Vec<u8>]) -> io::Result<()> {
        let mut buf = Vec::new();
        for (i, f) in frames.iter().enumerate() {
            let flags = if i + 1 < frames.len() { 1u8 } else { 0 };
            if f.len() > 255 {
                buf.push(flags | 2);
                buf.extend_from_slice(&(f.len() as u64).to_be_bytes());
            } else {
                buf.push(flags);
                buf.push(f.len() as u8);
            }
            buf.extend_from_slice(f);
        }
        self.stream.write_all(&buf)
    }

    /// The next whole message; commands in between are skipped.
    pub fn recv(&mut self) -> io::Result<Vec<Vec<u8>>> {
        let mut frames = Vec::new();
        loop {
            let (data, flags) = self.read_frame()?;
            if flags & 4 != 0 {
                continue;
            }
            frames.push(data);
            if flags & 1 == 0 {
                return Ok(frames);
            }
        }
    }
}
