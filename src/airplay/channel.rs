//! A connection to an AirPlay device: HTTP- and RTSP-shaped messages, in the clear at first,
//! then encrypted in frames once the pairing is verified.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::{Duration, Instant};

use super::crypto;
use crate::error::Result;
use crate::fail;

/// HomeKit encrypts at most this many bytes per frame.
const FRAME: usize = 1024;
const READ_SLICE: Duration = Duration::from_millis(250);
/// Nothing a device sends comes near this size.
const MAX_MESSAGE: usize = 4 * 1024 * 1024;

/// A request or a response: its first line, its headers and its body.
#[derive(Debug, PartialEq)]
pub struct Message {
    pub start: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Message {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(header, _)| header.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    /// The status of a response (`RTSP/1.0 200 OK`), 0 when it is not one.
    pub fn status(&self) -> u16 {
        self.start
            .split_whitespace()
            .nth(1)
            .and_then(|code| code.parse().ok())
            .unwrap_or(0)
    }
}

/// Takes one complete message off the front of the bytes: the message and its length.
pub fn parse(data: &[u8]) -> Option<(Message, usize)> {
    let end = data.windows(4).position(|window| window == b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&data[..end]);
    let mut lines = head.split("\r\n");
    let start = lines.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .collect();
    let length = headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.parse::<usize>().ok())
        .unwrap_or(0);
    let total = end + 4 + length;
    let body = data.get(end + 4..total)?.to_vec();
    Some((
        Message {
            start,
            headers,
            body,
        },
        total,
    ))
}

/// The bytes of a request or a response.
pub fn compose(start: &str, headers: &[(&str, String)], body: &[u8]) -> Vec<u8> {
    let mut text = format!("{start}\r\n");
    for (name, value) in headers {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    if !body.is_empty()
        || headers
            .iter()
            .all(|(name, _)| !name.eq_ignore_ascii_case("content-length"))
    {
        text.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    text.push_str("\r\n");
    let mut bytes = text.into_bytes();
    bytes.extend_from_slice(body);
    bytes
}

/// The two directions of an encrypted channel, each with its key and its frame counter.
pub struct Cipher {
    out_key: [u8; crypto::KEY],
    in_key: [u8; crypto::KEY],
    sent: u64,
    received: u64,
}

impl Cipher {
    pub fn new(out_key: [u8; crypto::KEY], in_key: [u8; crypto::KEY]) -> Cipher {
        Cipher {
            out_key,
            in_key,
            sent: 0,
            received: 0,
        }
    }

    /// Cuts data in frames: its length in the clear (and authenticated), then the sealed bytes.
    pub fn seal(&mut self, data: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(data.len() + (data.len() / FRAME + 1) * (2 + crypto::TAG));
        for frame in data.chunks(FRAME) {
            let length = (frame.len() as u16).to_le_bytes();
            out.extend_from_slice(&length);
            out.extend_from_slice(&crypto::seal(
                &self.out_key,
                crypto::nonce(self.sent.to_le_bytes()),
                &length,
                frame,
            )?);
            self.sent += 1;
        }
        Ok(out)
    }

    /// Opens every complete frame at the front of `raw`, leaving the rest for later.
    pub fn open(&mut self, raw: &mut Vec<u8>) -> Result<Vec<u8>> {
        let mut plain = Vec::new();
        while let Some(length) = raw.get(..2) {
            let length_bytes = [length[0], length[1]];
            let total = 2 + usize::from(u16::from_le_bytes(length_bytes)) + crypto::TAG;
            if raw.len() < total {
                break;
            }
            plain.extend_from_slice(&crypto::open(
                &self.in_key,
                crypto::nonce(self.received.to_le_bytes()),
                &length_bytes,
                &raw[2..total],
            )?);
            self.received += 1;
            raw.drain(..total);
        }
        Ok(plain)
    }
}

pub struct Connection {
    stream: TcpStream,
    cipher: Option<Cipher>,
    /// Bytes received and not yet decrypted.
    raw: Vec<u8>,
    /// Decrypted bytes not yet read as a message.
    plain: Vec<u8>,
}

impl Connection {
    pub fn open(address: SocketAddr, timeout: Duration) -> Result<Connection> {
        let stream = TcpStream::connect_timeout(&address, timeout)
            .map_err(|error| fail!("cannot reach the device at {address}: {error}"))?;
        stream.set_nodelay(true)?;
        stream.set_read_timeout(Some(READ_SLICE))?;
        stream.set_write_timeout(Some(timeout))?;
        Ok(Connection {
            stream,
            cipher: None,
            raw: Vec::new(),
            plain: Vec::new(),
        })
    }

    /// The address this machine uses on this connection.
    pub fn local_ip(&self) -> Result<String> {
        Ok(self.stream.local_addr()?.ip().to_string())
    }

    /// From now on everything is encrypted, in both directions.
    pub fn encrypt(&mut self, cipher: Cipher) {
        self.cipher = Some(cipher);
    }

    pub fn send(&mut self, data: &[u8]) -> Result<()> {
        match &mut self.cipher {
            Some(cipher) => {
                let sealed = cipher.seal(data)?;
                self.stream.write_all(&sealed)?;
            }
            None => self.stream.write_all(data)?,
        }
        Ok(())
    }

    /// The next message, or `None` when none arrived within `limit`.
    pub fn receive(&mut self, limit: Duration) -> Result<Option<Message>> {
        let deadline = Instant::now() + limit;
        loop {
            if let Some((message, used)) = parse(&self.plain) {
                self.plain.drain(..used);
                return Ok(Some(message));
            }
            if self.plain.len() > MAX_MESSAGE {
                return Err(fail!("the device sent an oversized message"));
            }
            let mut chunk = [0u8; 8192];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err(fail!("the device closed the connection")),
                Ok(size) => match &mut self.cipher {
                    Some(cipher) => {
                        self.raw.extend_from_slice(&chunk[..size]);
                        let opened = cipher.open(&mut self.raw)?;
                        self.plain.extend_from_slice(&opened);
                    }
                    None => self.plain.extend_from_slice(&chunk[..size]),
                },
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    if Instant::now() >= deadline {
                        return Ok(None);
                    }
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Sends a request and waits for its answer.
    pub fn exchange(&mut self, request: &[u8], limit: Duration) -> Result<Message> {
        self.send(request)?;
        self.receive(limit)?
            .ok_or_else(|| fail!("the device did not answer in time"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composes_and_parses() {
        let bytes = compose("POST /command RTSP/1.0", &[("CSeq", "3".into())], b"body");
        assert_eq!(
            bytes,
            b"POST /command RTSP/1.0\r\nCSeq: 3\r\nContent-Length: 4\r\n\r\nbody"
        );
        let (message, used) = parse(&bytes).unwrap();
        assert_eq!(used, bytes.len());
        assert_eq!(message.start, "POST /command RTSP/1.0");
        assert_eq!(message.header("cseq"), Some("3"));
        assert_eq!(message.header("missing"), None);
        assert_eq!(message.body, b"body");
        assert_eq!(message.status(), 0);

        let empty = compose("RTSP/1.0 200 OK", &[], b"");
        assert_eq!(empty, b"RTSP/1.0 200 OK\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(parse(&empty).unwrap().0.status(), 200);
        let fixed = compose("GET /info RTSP/1.0", &[("Content-Length", "0".into())], b"");
        assert_eq!(fixed, b"GET /info RTSP/1.0\r\nContent-Length: 0\r\n\r\n");
    }

    #[test]
    fn waits_for_whole_messages() {
        let bytes = compose("RTSP/1.0 200 OK", &[], b"0123456789");
        for cut in 0..bytes.len() {
            assert!(parse(&bytes[..cut]).is_none(), "{cut}");
        }
        let mut two = bytes.clone();
        two.extend_from_slice(&bytes);
        assert_eq!(parse(&two).unwrap().1, bytes.len());
        assert_eq!(
            parse(b"HTTP/1.1 204 No Content\r\n\r\n").unwrap().0.body,
            b""
        );
    }

    #[test]
    fn frames_round_trip_between_two_ends() {
        let (one, two) = ([1u8; crypto::KEY], [2u8; crypto::KEY]);
        let mut sender = Cipher::new(one, two);
        let mut receiver = Cipher::new(two, one);
        let message: Vec<u8> = (0..3000u32).map(|n| n as u8).collect();
        let sealed = sender.seal(&message).unwrap();
        assert_eq!(sealed.len(), 3000 + 3 * (2 + crypto::TAG));

        // Bytes arrive in arbitrary pieces.
        let mut raw = Vec::new();
        let mut opened = Vec::new();
        for piece in sealed.chunks(700) {
            raw.extend_from_slice(piece);
            opened.extend_from_slice(&receiver.open(&mut raw).unwrap());
        }
        assert_eq!(opened, message);
        assert!(raw.is_empty());

        // Counters advance: a frame replayed is refused.
        let mut replay = sealed[..2 + FRAME + crypto::TAG].to_vec();
        assert!(receiver.open(&mut replay).is_err());
        let second = sender.seal(b"again").unwrap();
        assert_eq!(receiver.open(&mut second.clone()).unwrap(), b"again");
        assert!(Cipher::new(one, two).open(&mut vec![1]).unwrap().is_empty());
    }
}
