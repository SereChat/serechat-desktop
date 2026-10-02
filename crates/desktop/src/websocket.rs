//! A minimal WebSocket client (RFC 6455) for the debugging connection to the
//! browser the agent drives: text messages out, text messages in, pings
//! answered, nothing more.
//!
//! ponytail: plain TCP to `127.0.0.1` only, no extensions, and the server's
//! `Sec-WebSocket-Accept` is not checked (that needs SHA-1). Enough for a
//! browser we started ourselves; a general client would need all three.

use std::io::{self, BufReader, ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Largest message accepted; screenshots arrive base64-encoded inside one.
const MAX_MESSAGE: usize = 64 << 20;
/// Largest handshake response accepted.
const MAX_HEADER: usize = 16 << 10;
/// How often a waiting read wakes up to check for cancellation.
const POLL: Duration = Duration::from_millis(100);

const OP_CONTINUATION: u8 = 0x0;
const OP_TEXT: u8 = 0x1;
const OP_BINARY: u8 = 0x2;
const OP_CLOSE: u8 = 0x8;
const OP_PING: u8 = 0x9;
const OP_PONG: u8 = 0xA;

/// What a read that gave up because of the user reports.
pub const STOPPED: &str = "Stopped by the user.";

/// An open WebSocket connection.
pub struct Socket {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Socket {
    /// Connects to `ws://127.0.0.1:{port}{path}`.
    ///
    /// # Errors
    /// The connection or the handshake failed.
    pub fn connect(port: u16, path: &str, cancel: &AtomicBool) -> Result<Self, String> {
        let stream = TcpStream::connect_timeout(&SocketAddr::from(([127, 0, 0, 1], port)), Duration::from_secs(5)).map_err(describe)?;
        stream.set_nodelay(true).map_err(describe)?;
        stream.set_read_timeout(Some(POLL)).map_err(describe)?;
        stream.set_write_timeout(Some(Duration::from_secs(10))).map_err(describe)?;
        let mut writer = stream.try_clone().map_err(describe)?;
        let nonce: Vec<u8> = (0..4).flat_map(|_| random_u32().to_le_bytes()).collect();
        let key = serechat::data_url("", &nonce).split_once(',').map(|(_, key)| key.to_owned()).unwrap_or_default();
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        writer.write_all(request.as_bytes()).map_err(describe)?;
        let mut reader = BufReader::new(stream);
        let header = read_header(&mut Patient::new(&mut reader, Duration::from_secs(10), cancel))?;
        let status = header.lines().next().unwrap_or_default();
        if status.split_whitespace().nth(1) != Some("101") {
            return Err(format!("The browser refused the connection ({status})."));
        }
        Ok(Self { reader, writer })
    }

    /// Sends a text message.
    ///
    /// # Errors
    /// The connection failed.
    pub fn send(&mut self, text: &str) -> Result<(), String> {
        write_frame(&mut self.writer, OP_TEXT, text.as_bytes(), random_u32().to_le_bytes()).map_err(describe)
    }

    /// Waits for the next text message, answering pings meanwhile, until
    /// `timeout` passes or `cancel` is raised. A message cut off that way
    /// leaves the connection unusable, so drop it after any error.
    ///
    /// # Errors
    /// Timed out, stopped, closed by the browser, or a broken connection.
    pub fn receive(&mut self, timeout: Duration, cancel: &AtomicBool) -> Result<String, String> {
        let mut message = Vec::new();
        loop {
            let frame = read_frame(&mut Patient::new(&mut self.reader, timeout, cancel), MAX_MESSAGE - message.len()).map_err(describe)?;
            match frame.opcode {
                OP_PING => write_frame(&mut self.writer, OP_PONG, &frame.payload, random_u32().to_le_bytes()).map_err(describe)?,
                OP_PONG => {}
                OP_CLOSE => return Err("The browser closed the connection.".into()),
                OP_TEXT | OP_BINARY | OP_CONTINUATION => {
                    message.extend_from_slice(&frame.payload);
                    if frame.fin {
                        return String::from_utf8(message).map_err(|_| "The browser sent a message that is not UTF-8.".into());
                    }
                }
                other => return Err(format!("The browser sent an unknown WebSocket frame ({other:#x}).")),
            }
        }
    }
}

/// A reader that waits through the socket's short read timeouts until its
/// own deadline, giving up early when `cancel` is raised.
struct Patient<'a, R> {
    inner: &'a mut R,
    deadline: Instant,
    cancel: &'a AtomicBool,
}

impl<'a, R> Patient<'a, R> {
    fn new(inner: &'a mut R, timeout: Duration, cancel: &'a AtomicBool) -> Self {
        Self { inner, deadline: Instant::now() + timeout, cancel }
    }
}

impl<R: Read> Read for Patient<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            match self.inner.read(buf) {
                Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {
                    // Not `Interrupted`: `read_exact` would retry that forever.
                    if self.cancel.load(Ordering::Relaxed) {
                        return Err(io::Error::other(STOPPED));
                    }
                    if Instant::now() >= self.deadline {
                        return Err(io::Error::new(ErrorKind::TimedOut, "The browser did not answer in time."));
                    }
                }
                other => return other,
            }
        }
    }
}

/// Reads an HTTP response header, up to and including the blank line.
fn read_header(reader: &mut impl Read) -> Result<String, String> {
    let mut header = Vec::new();
    let mut byte = [0u8];
    while !header.ends_with(b"\r\n\r\n") {
        if header.len() >= MAX_HEADER {
            return Err("The browser's handshake response is too long.".into());
        }
        reader.read_exact(&mut byte).map_err(describe)?;
        header.push(byte[0]);
    }
    Ok(String::from_utf8_lossy(&header).into_owned())
}

/// One WebSocket frame.
#[derive(Debug)]
struct Frame {
    /// The last frame of its message.
    fin: bool,
    opcode: u8,
    payload: Vec<u8>,
}

/// Reads one frame whose payload may be at most `limit` bytes.
fn read_frame(reader: &mut impl Read, limit: usize) -> io::Result<Frame> {
    let mut head = [0u8; 2];
    reader.read_exact(&mut head)?;
    let (fin, opcode, masked) = (head[0] & 0x80 != 0, head[0] & 0x0F, head[1] & 0x80 != 0);
    let len = match head[1] & 0x7F {
        126 => {
            let mut bytes = [0u8; 2];
            reader.read_exact(&mut bytes)?;
            u64::from(u16::from_be_bytes(bytes))
        }
        127 => {
            let mut bytes = [0u8; 8];
            reader.read_exact(&mut bytes)?;
            u64::from_be_bytes(bytes)
        }
        n => u64::from(n),
    };
    let len = usize::try_from(len)
        .ok()
        .filter(|&n| n <= limit)
        .ok_or_else(|| io::Error::new(ErrorKind::InvalidData, "The browser sent a message that is too large."))?;
    let mut mask = [0u8; 4];
    if masked {
        reader.read_exact(&mut mask)?;
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload)?;
    if masked {
        payload.iter_mut().enumerate().for_each(|(i, b)| *b ^= mask[i % 4]);
    }
    Ok(Frame { fin, opcode, payload })
}

/// Writes one final frame, masked with `mask` as clients must.
fn write_frame(writer: &mut impl Write, opcode: u8, payload: &[u8], mask: [u8; 4]) -> io::Result<()> {
    let mut frame = Vec::with_capacity(payload.len() + 14);
    frame.push(0x80 | opcode);
    // The length takes 7 bits, or 0x7E then 16 bits, or 0x7F then 64 bits.
    let len = payload.len();
    if len < 0x7E {
        frame.push(0x80 | len as u8);
    } else if let Ok(len) = u16::try_from(len) {
        frame.push(0x80 | 0x7E);
        frame.extend_from_slice(&len.to_be_bytes());
    } else {
        frame.push(0x80 | 0x7F);
        frame.extend_from_slice(&(len as u64).to_be_bytes());
    }
    frame.extend_from_slice(&mask);
    frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
    writer.write_all(&frame)?;
    writer.flush()
}

/// A connection error as the model reads it.
fn describe(e: io::Error) -> String {
    match e.kind() {
        ErrorKind::UnexpectedEof | ErrorKind::ConnectionReset | ErrorKind::ConnectionAborted | ErrorKind::BrokenPipe => {
            "The connection to the browser was lost.".into()
        }
        // Our own messages (timeouts, stops) read best as they are.
        ErrorKind::TimedOut | ErrorKind::Other | ErrorKind::InvalidData if e.get_ref().is_some() => {
            e.into_inner().map_or_else(String::new, |inner| inner.to_string())
        }
        _ => format!("The connection to the browser failed: {e}"),
    }
}

/// A random number, good enough for masks and handshake nonces (which only
/// need to be unpredictable to proxies; there are none on localhost).
fn random_u32() -> u32 {
    use std::hash::{BuildHasher, RandomState};
    RandomState::new().hash_one(Instant::now()) as u32
}


#[cfg(test)]
mod tests {
    use super::*;

    fn round_trip(len: usize) {
        let payload: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();
        let mut wire = Vec::new();
        write_frame(&mut wire, OP_TEXT, &payload, [1, 2, 3, 4]).unwrap();
        let frame = read_frame(&mut wire.as_slice(), usize::MAX).unwrap();
        assert!(frame.fin && frame.opcode == OP_TEXT && frame.payload == payload, "length {len}");
    }

    #[test]
    fn frames_round_trip_at_every_length_encoding() {
        for len in [0, 125, 126, 65_535, 65_536, 200_000] {
            round_trip(len);
        }
    }

    #[test]
    fn hostile_frames_are_refused() {
        // A 64-bit length far past the limit is refused before allocating.
        let mut huge = vec![0x81, 127];
        huge.extend_from_slice(&u64::MAX.to_be_bytes());
        assert_eq!(read_frame(&mut huge.as_slice(), MAX_MESSAGE).err().map(|e| e.kind()), Some(ErrorKind::InvalidData));
        // A frame cut off mid-payload is an error, not a short message.
        assert!(read_frame(&mut [0x81, 5, b'a'].as_slice(), MAX_MESSAGE).is_err());
        assert!(read_header(&mut vec![b'x'; MAX_HEADER + 10].as_slice()).is_err());
        assert_eq!(read_header(&mut b"HTTP/1.1 101 OK\r\n\r\nrest".as_slice()).unwrap(), "HTTP/1.1 101 OK\r\n\r\n");
    }

    #[test]
    fn waiting_reads_stop_when_asked() {
        struct Silent;
        impl Read for Silent {
            fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
                Err(ErrorKind::WouldBlock.into())
            }
        }
        let stop = AtomicBool::new(true);
        let err = read_frame(&mut Patient::new(&mut Silent, Duration::from_secs(60), &stop), 10).unwrap_err();
        assert_eq!(describe(err), STOPPED);
        let go = AtomicBool::new(false);
        let err = read_frame(&mut Patient::new(&mut Silent, Duration::ZERO, &go), 10).unwrap_err();
        assert_eq!(describe(err), "The browser did not answer in time.");
    }
}
