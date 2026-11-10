//! A small HTTP/1.1 client: enough for UPnP descriptions and SOAP calls on a local network.

use std::io::{Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use crate::error::Result;
use crate::fail;

/// Devices answer with a few kilobytes; anything far larger is not a description.
const MAX_RESPONSE: usize = 4 * 1024 * 1024;

#[derive(Debug, PartialEq)]
pub struct Url {
    pub host: String,
    pub port: u16,
    pub path: String,
}

impl Url {
    /// Reads an `http://host[:port][/path]` URL.
    pub fn parse(text: &str) -> Result<Url> {
        let rest = text
            .strip_prefix("http://")
            .ok_or_else(|| fail!("unsupported URL {text}"))?;
        let (authority, path) = match rest.find('/') {
            Some(slash) => (&rest[..slash], &rest[slash..]),
            None => (rest, "/"),
        };
        let (host, port) = match authority.rsplit_once(':') {
            Some((host, port)) => (
                host,
                port.parse().map_err(|_| fail!("invalid port in {text}"))?,
            ),
            None => (authority, 80),
        };
        if host.is_empty() {
            return Err(fail!("missing host in {text}"));
        }
        Ok(Url {
            host: host.to_string(),
            port,
            path: path.to_string(),
        })
    }

    pub fn authority(&self) -> String {
        format!("{}:{}", self.host, self.port)
    }

    /// Resolves a reference found in a document served at this URL.
    pub fn join(&self, reference: &str) -> String {
        if reference.starts_with("http://") {
            return reference.to_string();
        }
        if reference.starts_with('/') {
            return format!("http://{}{}", self.authority(), reference);
        }
        let directory = match self.path.rfind('/') {
            Some(slash) => &self.path[..=slash],
            None => "/",
        };
        format!("http://{}{}{}", self.authority(), directory, reference)
    }
}

#[derive(Debug, PartialEq)]
pub struct Response {
    pub status: u16,
    pub body: Vec<u8>,
}

/// Sends one request and reads the whole answer.
pub fn request(
    method: &str,
    url: &str,
    headers: &[(&str, &str)],
    body: &[u8],
    timeout: Duration,
) -> Result<Response> {
    let target = Url::parse(url)?;
    let address = (target.host.as_str(), target.port)
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| fail!("cannot resolve {}", target.host))?;
    let mut stream = TcpStream::connect_timeout(&address, timeout)?;
    stream.set_read_timeout(Some(timeout))?;
    stream.set_write_timeout(Some(timeout))?;

    let mut head = format!(
        "{method} {} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\nUser-Agent: dapple/{}\r\n",
        target.path,
        target.authority(),
        env!("CARGO_PKG_VERSION")
    );
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    if method != "GET" {
        head.push_str(&format!("Content-Length: {}\r\n", body.len()));
    }
    head.push_str("\r\n");
    stream.write_all(head.as_bytes())?;
    stream.write_all(body)?;

    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    loop {
        if let Some(response) = parse_response(&raw, false) {
            return Ok(response);
        }
        let read = stream.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        raw.extend_from_slice(&chunk[..read]);
        if raw.len() > MAX_RESPONSE {
            return Err(fail!("response from {url} is too large"));
        }
    }
    parse_response(&raw, true).ok_or_else(|| fail!("incomplete response from {url}"))
}

/// Reads a response once it is complete; `end` means the peer closed the connection.
pub fn parse_response(raw: &[u8], end: bool) -> Option<Response> {
    let split = find(raw, b"\r\n\r\n")?;
    let head = String::from_utf8_lossy(&raw[..split]);
    let body = &raw[split + 4..];
    let mut lines = head.split("\r\n");
    let status: u16 = lines.next()?.split_whitespace().nth(1)?.parse().ok()?;
    let mut length = None;
    let mut chunked = false;
    for line in lines {
        let Some((name, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim();
        match name.trim().to_ascii_lowercase().as_str() {
            "content-length" => length = value.parse::<usize>().ok(),
            "transfer-encoding" => chunked = value.to_ascii_lowercase().contains("chunked"),
            _ => {}
        }
    }
    if chunked {
        let (decoded, complete) = decode_chunked(body);
        return (complete || end).then_some(Response {
            status,
            body: decoded,
        });
    }
    match length {
        Some(length) if body.len() >= length => Some(Response {
            status,
            body: body[..length].to_vec(),
        }),
        _ if end => Some(Response {
            status,
            body: body.to_vec(),
        }),
        _ => None,
    }
}

/// Decodes a chunked body; the flag tells whether the final empty chunk was seen.
fn decode_chunked(mut body: &[u8]) -> (Vec<u8>, bool) {
    let mut decoded = Vec::new();
    loop {
        let Some(line_end) = find(body, b"\r\n") else {
            return (decoded, false);
        };
        let line = String::from_utf8_lossy(&body[..line_end]);
        let digits = line.split(';').next().unwrap_or("").trim();
        let Ok(size) = usize::from_str_radix(digits, 16) else {
            return (decoded, false);
        };
        if size == 0 {
            return (decoded, true);
        }
        let start = line_end + 2;
        let Some(chunk) = body.get(start..start + size) else {
            return (decoded, false);
        };
        decoded.extend_from_slice(chunk);
        body = body.get(start + size + 2..).unwrap_or(&[]);
    }
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls() {
        let url = Url::parse("http://192.168.1.20:8080/dmr/desc.xml").unwrap();
        assert_eq!(url.host, "192.168.1.20");
        assert_eq!(url.port, 8080);
        assert_eq!(url.path, "/dmr/desc.xml");
        assert_eq!(Url::parse("http://tv").unwrap().path, "/");
        assert_eq!(Url::parse("http://tv").unwrap().port, 80);
        assert!(Url::parse("https://tv").is_err());
        assert!(Url::parse("http://tv:port/").is_err());
        assert!(Url::parse("http://:80/").is_err());
        assert_eq!(url.join("/ctl/AV"), "http://192.168.1.20:8080/ctl/AV");
        assert_eq!(url.join("ctl/AV"), "http://192.168.1.20:8080/dmr/ctl/AV");
        assert_eq!(url.join("http://other/x"), "http://other/x");
    }

    #[test]
    fn responses() {
        let ok = |status, body: &[u8]| {
            Some(Response {
                status,
                body: body.to_vec(),
            })
        };
        let full = b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello";
        assert_eq!(parse_response(full, false), ok(200, b"hello"));
        assert_eq!(parse_response(&full[..full.len() - 2], false), None);
        assert_eq!(parse_response(b"HTTP/1.1 200 OK\r\n", true), None);
        let close = b"HTTP/1.0 500 Oops\r\nServer: x\r\n\r\nfault";
        assert_eq!(parse_response(close, false), None);
        assert_eq!(parse_response(close, true), ok(500, b"fault"));
        let chunked =
            b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;x=y\r\nde\r\n0\r\n\r\n";
        assert_eq!(parse_response(chunked, false), ok(200, b"abcde"));
        assert_eq!(parse_response(&chunked[..chunked.len() - 8], false), None);
        assert_eq!(parse_response(b"garbage\r\n\r\n", true), None);
    }
}
