//! The HTTP server screens fetch the video from: one file, behind an unguessable path, with
//! byte ranges so that TVs can seek. Nothing else on the machine is reachable through it.

use std::collections::hash_map::RandomState;
use std::fs::File;
use std::hash::{BuildHasher, Hasher};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use crate::dlna;
use crate::error::Result;
use crate::fail;
use crate::media::{self, Convert, Encoder, Probe};

const MAX_HEAD: usize = 16 * 1024;

/// What the server offers.
pub struct Source {
    pub path: PathBuf,
    pub mime: String,
    /// How to convert the file on the fly, when ffmpeg is there to do it.
    pub conversion: Option<Conversion>,
    pub vtt: Option<String>,
    pub srt: Option<String>,
    pub verbose: bool,
}

pub struct Conversion {
    pub convert: Convert,
    pub probe: Option<Probe>,
    pub encoder: Encoder,
}

pub struct MediaServer {
    port: u16,
    token: String,
}

impl MediaServer {
    pub fn port(&self) -> u16 {
        self.port
    }

    /// The path of something served, below the secret prefix.
    pub fn path(&self, name: &str) -> String {
        format!("/{}/{name}", self.token)
    }
}

/// Starts serving in the background, for as long as the program runs.
pub fn start(bind: SocketAddr, source: Source) -> Result<MediaServer> {
    let listener =
        TcpListener::bind(bind).map_err(|error| fail!("cannot listen on {bind}: {error}"))?;
    let port = listener.local_addr()?.port();
    let token = random_token();
    let shared = Arc::new((source, token.clone()));
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let shared = Arc::clone(&shared);
            // TVs open several connections at once (probing, then playing): one thread each.
            thread::spawn(move || {
                let (source, token) = &*shared;
                if let Err(error) = serve(stream, source, token) {
                    if source.verbose {
                        eprintln!("[server] {error}");
                    }
                }
            });
        }
    });
    Ok(MediaServer { port, token })
}

/// 128 bits from the operating system's randomness, through the standard hasher's seeds.
fn random_token() -> String {
    let word = || RandomState::new().build_hasher().finish();
    format!("{:016x}{:016x}", word(), word())
}

#[derive(Debug, PartialEq)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub query: String,
    pub range: Option<String>,
}

/// Reads the request line and the only header that matters here.
pub fn parse_request(head: &str) -> Option<Request> {
    let mut lines = head.split("\r\n");
    let mut first = lines.next()?.split(' ');
    let method = first.next()?.to_string();
    let target = first.next()?;
    first.next()?.starts_with("HTTP/").then_some(())?;
    let (path, query) = target.split_once('?').unwrap_or((target, ""));
    let range = lines.find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.trim()
            .eq_ignore_ascii_case("range")
            .then(|| value.trim().to_string())
    });
    Some(Request {
        method,
        path: path.to_string(),
        query: query.to_string(),
        range,
    })
}

#[derive(Debug, PartialEq)]
pub enum Route {
    File,
    Stream,
    Vtt,
    Srt,
}

/// What a path designates: `/<token>/<name>.<extension>`, the extension being for the device.
pub fn route(path: &str, token: &str) -> Option<Route> {
    let name = path
        .strip_prefix('/')?
        .strip_prefix(token)?
        .strip_prefix('/')?;
    match name.split('.').next()? {
        "file" => Some(Route::File),
        "stream" => Some(Route::Stream),
        "subtitles" if name.ends_with(".vtt") => Some(Route::Vtt),
        "subtitles" if name.ends_with(".srt") => Some(Route::Srt),
        _ => None,
    }
}

#[derive(Debug, PartialEq)]
pub enum Range {
    Whole,
    /// First and last byte, inclusive.
    Part(u64, u64),
    Unsatisfiable,
}

/// Reads a `Range` header for a file of `size` bytes. Several ranges at once are not worth
/// supporting: the whole file answers them, as the specification allows.
pub fn parse_range(header: Option<&str>, size: u64) -> Range {
    let Some(spec) = header.and_then(|header| header.trim().strip_prefix("bytes=")) else {
        return Range::Whole;
    };
    let Some((first, last)) = spec.split_once('-') else {
        return Range::Whole;
    };
    if spec.contains(',') {
        return Range::Whole;
    }
    let (first, last) = (first.trim(), last.trim());
    if first.is_empty() {
        // The last N bytes.
        return match last.parse::<u64>() {
            Ok(0) => Range::Unsatisfiable,
            Ok(count) if size > 0 => Range::Part(size.saturating_sub(count), size - 1),
            Ok(_) => Range::Unsatisfiable,
            Err(_) => Range::Whole,
        };
    }
    let Ok(first) = first.parse::<u64>() else {
        return Range::Whole;
    };
    if first >= size {
        return Range::Unsatisfiable;
    }
    let last = match last.parse::<u64>() {
        Ok(last) if last < first => return Range::Whole,
        Ok(last) => last.min(size - 1),
        Err(_) if last.is_empty() => size - 1,
        Err(_) => return Range::Whole,
    };
    Range::Part(first, last)
}

/// The `start` of a converted stream, from the query string.
pub fn start_of(query: &str) -> f64 {
    query
        .split('&')
        .find_map(|pair| pair.strip_prefix("start="))
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|start| start.is_finite() && *start > 0.0)
        .unwrap_or(0.0)
}

fn read_head(stream: &mut TcpStream) -> io::Result<String> {
    let mut head = Vec::new();
    let mut byte = [0u8; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = stream.read(&mut byte)?;
        if read == 0 || head.len() > MAX_HEAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "incomplete request",
            ));
        }
        head.extend_from_slice(&byte[..read]);
    }
    Ok(String::from_utf8_lossy(&head).into_owned())
}

/// Headers every answer carries: Cast players fetch with CORS, and nothing is kept alive.
const COMMON: &str = "Server: dapple\r\nConnection: close\r\nAccess-Control-Allow-Origin: *\r\n\
                      Access-Control-Allow-Headers: Range, Content-Type\r\n\
                      Access-Control-Expose-Headers: Content-Range, Content-Length, Accept-Ranges\r\n";

fn answer(stream: &mut TcpStream, status: &str, headers: &str) -> io::Result<()> {
    stream.write_all(format!("HTTP/1.1 {status}\r\n{COMMON}{headers}\r\n").as_bytes())
}

fn serve(mut stream: TcpStream, source: &Source, token: &str) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    let head = read_head(&mut stream)?;
    let Some(request) = parse_request(&head) else {
        return answer(&mut stream, "400 Bad Request", "Content-Length: 0\r\n");
    };
    if source.verbose {
        eprintln!(
            "[server] {} {} {}",
            request.method,
            request.path.replace(token, "…"),
            request.range.as_deref().unwrap_or("")
        );
    }
    let body = match request.method.as_str() {
        "GET" => true,
        "HEAD" => false,
        "OPTIONS" => {
            return answer(
                &mut stream,
                "204 No Content",
                "Access-Control-Allow-Methods: GET, HEAD, OPTIONS\r\n",
            )
        }
        _ => {
            return answer(
                &mut stream,
                "405 Method Not Allowed",
                "Content-Length: 0\r\n",
            )
        }
    };
    let text = |stream: &mut TcpStream, content: &Option<String>, mime: &str| match content {
        Some(content) => {
            answer(
                stream,
                "200 OK",
                &format!(
                    "Content-Type: {mime}\r\nContent-Length: {}\r\n",
                    content.len()
                ),
            )?;
            if body {
                stream.write_all(content.as_bytes())?;
            }
            Ok(())
        }
        None => answer(stream, "404 Not Found", "Content-Length: 0\r\n"),
    };
    match (route(&request.path, token), &source.conversion) {
        (Some(Route::File), _) => send_file(&mut stream, source, request.range.as_deref(), body),
        (Some(Route::Stream), Some(conversion)) => send_conversion(
            &mut stream,
            source,
            conversion,
            start_of(&request.query),
            body,
        ),
        (Some(Route::Vtt), _) => text(&mut stream, &source.vtt, "text/vtt; charset=utf-8"),
        (Some(Route::Srt), _) => text(&mut stream, &source.srt, "text/srt; charset=utf-8"),
        _ => answer(&mut stream, "404 Not Found", "Content-Length: 0\r\n"),
    }
}

fn send_file(
    stream: &mut TcpStream,
    source: &Source,
    range: Option<&str>,
    body: bool,
) -> io::Result<()> {
    let mut file = File::open(&source.path)?;
    let size = file.metadata()?.len();
    let dlna = format!(
        "Content-Type: {}\r\nAccept-Ranges: bytes\r\ntransferMode.dlna.org: Streaming\r\ncontentFeatures.dlna.org: {}\r\n",
        source.mime,
        dlna::content_features(true)
    );
    let (first, last) = match parse_range(range, size) {
        Range::Whole => {
            answer(
                stream,
                "200 OK",
                &format!("{dlna}Content-Length: {size}\r\n"),
            )?;
            (0, size.saturating_sub(1))
        }
        Range::Part(first, last) => {
            answer(
                stream,
                "206 Partial Content",
                &format!(
                    "{dlna}Content-Range: bytes {first}-{last}/{size}\r\nContent-Length: {}\r\n",
                    last - first + 1
                ),
            )?;
            (first, last)
        }
        Range::Unsatisfiable => {
            return answer(
                stream,
                "416 Range Not Satisfiable",
                &format!("Content-Range: bytes */{size}\r\nContent-Length: 0\r\n"),
            );
        }
    };
    if body && size > 0 {
        file.seek(SeekFrom::Start(first))?;
        // The standard library hands this copy to the kernel where it can (sendfile on Linux).
        io::copy(&mut file.take(last - first + 1), stream)?;
    }
    Ok(())
}

fn send_conversion(
    stream: &mut TcpStream,
    source: &Source,
    conversion: &Conversion,
    start: f64,
    body: bool,
) -> io::Result<()> {
    // No length: the stream ends when ffmpeg does, and the connection closes.
    answer(
        stream,
        "200 OK",
        &format!(
            "Content-Type: {}\r\ntransferMode.dlna.org: Streaming\r\ncontentFeatures.dlna.org: {}\r\n",
            conversion.convert.mime(),
            dlna::content_features(false)
        ),
    )?;
    if !body {
        return Ok(());
    }
    let arguments = media::ffmpeg_args(
        &source.path,
        conversion.probe.as_ref(),
        &conversion.convert,
        start,
        conversion.encoder,
    );
    let mut ffmpeg = Command::new("ffmpeg")
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(if source.verbose {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .spawn()?;
    let copied = match ffmpeg.stdout.take() {
        Some(mut output) => io::copy(&mut output, stream).map(drop),
        None => Ok(()),
    };
    // The device went away or the film ended: either way the encoder must not outlive it.
    let _ = ffmpeg.kill();
    let _ = ffmpeg.wait();
    copied
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_requests() {
        let request = parse_request(
            "GET /tok/file.mkv?start=3 HTTP/1.1\r\nHost: x\r\nrange: bytes=0-\r\n\r\n",
        )
        .unwrap();
        assert_eq!(
            request,
            Request {
                method: "GET".into(),
                path: "/tok/file.mkv".into(),
                query: "start=3".into(),
                range: Some("bytes=0-".into())
            }
        );
        let head = parse_request("HEAD / HTTP/1.0\r\n\r\n").unwrap();
        assert_eq!(
            (head.method.as_str(), head.query.as_str(), head.range),
            ("HEAD", "", None)
        );
        for bad in ["", "GET", "GET /", "GET / FTP/1"] {
            assert_eq!(parse_request(bad), None, "{bad}");
        }
    }

    #[test]
    fn routes() {
        assert_eq!(route("/tok/file.mkv", "tok"), Some(Route::File));
        assert_eq!(route("/tok/stream.ts", "tok"), Some(Route::Stream));
        assert_eq!(route("/tok/subtitles.vtt", "tok"), Some(Route::Vtt));
        assert_eq!(route("/tok/subtitles.srt", "tok"), Some(Route::Srt));
        for other in [
            "/tok/subtitles.txt",
            "/tok/other",
            "/bad/file.mkv",
            "/tokfile",
            "tok/file",
            "/",
            "/tok",
        ] {
            assert_eq!(route(other, "tok"), None, "{other}");
        }
    }

    #[test]
    fn ranges() {
        assert_eq!(parse_range(None, 100), Range::Whole);
        assert_eq!(parse_range(Some("bytes=0-"), 100), Range::Part(0, 99));
        assert_eq!(parse_range(Some("bytes=10-19"), 100), Range::Part(10, 19));
        assert_eq!(parse_range(Some(" bytes=90-500"), 100), Range::Part(90, 99));
        assert_eq!(parse_range(Some("bytes=-10"), 100), Range::Part(90, 99));
        assert_eq!(parse_range(Some("bytes=-500"), 100), Range::Part(0, 99));
        assert_eq!(parse_range(Some("bytes=100-"), 100), Range::Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=-0"), 100), Range::Unsatisfiable);
        assert_eq!(parse_range(Some("bytes=-5"), 0), Range::Unsatisfiable);
        for whole in [
            "items=0-5",
            "bytes=5",
            "bytes=0-1,5-6",
            "bytes=a-5",
            "bytes=5-a",
            "bytes=-a",
            "bytes=9-3",
        ] {
            assert_eq!(parse_range(Some(whole), 100), Range::Whole, "{whole}");
        }
    }

    #[test]
    fn stream_starts() {
        assert_eq!(start_of("start=12.5"), 12.5);
        assert_eq!(start_of("a=1&start=7"), 7.0);
        for none in ["", "start=", "start=-4", "start=x", "start=inf", "begin=3"] {
            assert_eq!(start_of(none), 0.0, "{none}");
        }
    }

    #[test]
    fn tokens_differ() {
        let (one, two) = (random_token(), random_token());
        assert_eq!(one.len(), 32);
        assert_ne!(one, two);
    }

    #[test]
    fn serves_a_file_with_ranges() {
        let path = std::env::temp_dir().join(format!("dapple-test-{}.bin", random_token()));
        std::fs::write(&path, b"0123456789").unwrap();
        let source = Source {
            path: path.clone(),
            mime: "video/mp4".into(),
            conversion: None,
            vtt: Some("WEBVTT\n".into()),
            srt: None,
            verbose: false,
        };
        let server = start("127.0.0.1:0".parse().unwrap(), source).unwrap();
        let get = |name: &str, range: Option<&str>| {
            let mut stream = TcpStream::connect(("127.0.0.1", server.port())).unwrap();
            let range = range
                .map(|range| format!("Range: {range}\r\n"))
                .unwrap_or_default();
            write!(stream, "GET {} HTTP/1.1\r\nHost: x\r\n{range}\r\n", name).unwrap();
            let mut answer = String::new();
            stream.read_to_string(&mut answer).unwrap();
            answer
        };
        let whole = get(&server.path("file.mp4"), None);
        assert!(
            whole.starts_with("HTTP/1.1 200 OK\r\n") && whole.ends_with("\r\n\r\n0123456789"),
            "{whole}"
        );
        assert!(
            whole.contains("Content-Type: video/mp4\r\n")
                && whole.contains("Accept-Ranges: bytes\r\n")
        );
        let part = get(&server.path("file.mp4"), Some("bytes=2-5"));
        assert!(
            part.starts_with("HTTP/1.1 206 ")
                && part.contains("Content-Range: bytes 2-5/10\r\n")
                && part.ends_with("\r\n\r\n2345"),
            "{part}"
        );
        assert!(get(&server.path("file.mp4"), Some("bytes=50-")).starts_with("HTTP/1.1 416 "));
        assert!(get(&server.path("subtitles.vtt"), None).ends_with("\r\n\r\nWEBVTT\n"));
        assert!(get(&server.path("subtitles.srt"), None).starts_with("HTTP/1.1 404 "));
        assert!(get(&server.path("stream.mp4"), None).starts_with("HTTP/1.1 404 "));
        assert!(get("/guess/file.mp4", None).starts_with("HTTP/1.1 404 "));
        std::fs::remove_file(path).unwrap();
    }
}
