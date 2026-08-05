//! Minimal RFC 6455 WebSocket server framing.
//!
//! Only what a VNC client needs: a binary stream in both directions, plus
//! ping/pong and close handling. The two halves are exposed as `Read` and
//! `Write` so the RFB code does not know it is on a WebSocket at all.

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

use sha1::{Digest, Sha1};

const MAGIC: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// Refuse frames larger than this so a client cannot exhaust memory.
const MAX_FRAME: u64 = 8 << 20;

const OP_CONTINUATION: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xa;

/// `Sec-WebSocket-Accept` for a client's `Sec-WebSocket-Key`.
pub fn accept_key(key: &str) -> String {
    let mut h = Sha1::new();
    h.update(key.as_bytes());
    h.update(MAGIC.as_bytes());
    base64(&h.finalize())
}

pub fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(ALPHABET[(n >> 18 & 63) as usize] as char);
        out.push(ALPHABET[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { ALPHABET[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { ALPHABET[(n & 63) as usize] as char } else { '=' });
    }
    out
}

/// The write half: every `write` call becomes one binary frame.
pub struct WsSink<W: Write> {
    inner: W,
    closed: bool,
}

impl<W: Write> WsSink<W> {
    pub fn new(inner: W) -> Self {
        WsSink { inner, closed: false }
    }

    fn frame(&mut self, opcode: u8, payload: &[u8]) -> io::Result<()> {
        if self.closed {
            return Err(io::Error::new(io::ErrorKind::BrokenPipe, "websocket closed"));
        }
        let mut head = Vec::with_capacity(10);
        head.push(0x80 | opcode);
        match payload.len() {
            n if n < 126 => head.push(n as u8),
            n if n <= u16::MAX as usize => {
                head.push(126);
                head.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                head.push(127);
                head.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        self.inner.write_all(&head)?;
        self.inner.write_all(payload)?;
        Ok(())
    }

    pub fn pong(&mut self, payload: &[u8]) -> io::Result<()> {
        self.frame(OP_PONG, payload)?;
        self.inner.flush()
    }

    pub fn close(&mut self) -> io::Result<()> {
        if !self.closed {
            let _ = self.frame(OP_CLOSE, &1000u16.to_be_bytes());
            let _ = self.inner.flush();
            self.closed = true;
        }
        Ok(())
    }
}

impl<W: Write> Write for WsSink<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Zero-length writes would produce an empty frame with no meaning.
        if buf.is_empty() {
            return Ok(0);
        }
        self.frame(OP_BINARY, buf)?;
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

/// The read half: unmasks incoming frames and hands back a plain byte stream.
pub struct WsSource<R: Read, W: Write> {
    inner: R,
    sink: Arc<Mutex<WsSink<W>>>,
    payload: Vec<u8>,
    pos: usize,
    eof: bool,
}

impl<R: Read, W: Write> WsSource<R, W> {
    pub fn new(inner: R, sink: Arc<Mutex<WsSink<W>>>) -> Self {
        WsSource { inner, sink, payload: Vec::new(), pos: 0, eof: false }
    }

    /// Read frames until one carries application data.
    fn next_data_frame(&mut self) -> io::Result<bool> {
        loop {
            if self.eof {
                return Ok(false);
            }
            let mut head = [0u8; 2];
            match self.inner.read_exact(&mut head) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => {
                    self.eof = true;
                    return Ok(false);
                }
                Err(e) => return Err(e),
            }
            let opcode = head[0] & 0x0f;
            let masked = head[1] & 0x80 != 0;
            let mut len = (head[1] & 0x7f) as u64;
            if len == 126 {
                let mut b = [0u8; 2];
                self.inner.read_exact(&mut b)?;
                len = u16::from_be_bytes(b) as u64;
            } else if len == 127 {
                let mut b = [0u8; 8];
                self.inner.read_exact(&mut b)?;
                len = u64::from_be_bytes(b);
            }
            if len > MAX_FRAME {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "websocket frame too large"));
            }
            let mut mask = [0u8; 4];
            if masked {
                self.inner.read_exact(&mut mask)?;
            }
            let mut payload = vec![0u8; len as usize];
            self.inner.read_exact(&mut payload)?;
            if masked {
                for (i, b) in payload.iter_mut().enumerate() {
                    *b ^= mask[i % 4];
                }
            }

            match opcode {
                OP_CONTINUATION | OP_TEXT | OP_BINARY => {
                    if payload.is_empty() {
                        continue;
                    }
                    self.payload = payload;
                    self.pos = 0;
                    return Ok(true);
                }
                OP_PING => {
                    let _ = self.sink.lock().unwrap().pong(&payload);
                }
                OP_PONG => {}
                OP_CLOSE => {
                    let _ = self.sink.lock().unwrap().close();
                    self.eof = true;
                    return Ok(false);
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "unknown websocket opcode",
                    ))
                }
            }
        }
    }
}

impl<R: Read, W: Write> Read for WsSource<R, W> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        while self.pos >= self.payload.len() {
            if !self.next_data_frame()? {
                return Ok(0);
            }
        }
        let n = buf.len().min(self.payload.len() - self.pos);
        buf[..n].copy_from_slice(&self.payload[self.pos..self.pos + n]);
        self.pos += n;
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_known_values() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }

    #[test]
    fn accept_key_matches_rfc6455_example() {
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    /// Build a masked client frame the way a browser would.
    fn client_frame(opcode: u8, payload: &[u8]) -> Vec<u8> {
        let mask = [0x11u8, 0x22, 0x33, 0x44];
        let mut out = vec![0x80 | opcode];
        assert!(payload.len() < 126);
        out.push(0x80 | payload.len() as u8);
        out.extend_from_slice(&mask);
        for (i, b) in payload.iter().enumerate() {
            out.push(b ^ mask[i % 4]);
        }
        out
    }

    #[test]
    fn reads_unmasked_payload_across_frames() {
        let mut wire = client_frame(OP_BINARY, b"RFB 003.008\n");
        wire.extend(client_frame(OP_BINARY, b"hello"));
        let sink = Arc::new(Mutex::new(WsSink::new(Vec::new())));
        let mut src = WsSource::new(io::Cursor::new(wire), sink);
        let mut out = Vec::new();
        src.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"RFB 003.008\nhello");
    }

    #[test]
    fn ping_is_answered_and_close_ends_the_stream() {
        let mut wire = client_frame(OP_PING, b"hi");
        wire.extend(client_frame(OP_BINARY, b"data"));
        wire.extend(client_frame(OP_CLOSE, &[]));
        wire.extend(client_frame(OP_BINARY, b"ignored"));
        let sink = Arc::new(Mutex::new(WsSink::new(Vec::new())));
        let mut src = WsSource::new(io::Cursor::new(wire), sink.clone());
        let mut out = Vec::new();
        src.read_to_end(&mut out).unwrap();
        assert_eq!(out, b"data");
        let written = std::mem::take(&mut sink.lock().unwrap().inner);
        assert_eq!(written[0], 0x80 | OP_PONG);
        assert_eq!(&written[2..4], b"hi");
    }

    #[test]
    fn writes_are_single_binary_frames() {
        let mut sink = WsSink::new(Vec::new());
        sink.write_all(&[1, 2, 3]).unwrap();
        assert_eq!(sink.inner, vec![0x82, 3, 1, 2, 3]);

        let mut big = WsSink::new(Vec::new());
        big.write_all(&vec![0u8; 200]).unwrap();
        assert_eq!(big.inner[0], 0x82);
        assert_eq!(big.inner[1], 126);
        assert_eq!(u16::from_be_bytes([big.inner[2], big.inner[3]]), 200);
    }

    #[test]
    fn oversized_frames_are_rejected() {
        let wire = vec![0x82, 0xff, 0, 0, 0, 1, 0, 0, 0, 0];
        let sink = Arc::new(Mutex::new(WsSink::new(Vec::new())));
        let mut src = WsSource::new(io::Cursor::new(wire), sink);
        let mut out = Vec::new();
        assert!(src.read_to_end(&mut out).is_err());
    }
}
