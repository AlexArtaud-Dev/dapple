//! A discovery relay: lists the screens of the network over HTTP, for programs that cannot see
//! them themselves (a container on a bridge network receives no multicast).
//!
//! It answers one question, `GET /devices`, and nothing else: it plays nothing and reads no file.

use std::io::Write;
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::discovery::{self, Device};
use crate::error::Result;
use crate::fail;
use crate::http;
use crate::server;

/// The port relays listen on, and where one is looked for.
pub const PORT: u16 = 8691;

/// Searching takes seconds: forms opened one after the other share one search.
const FRESH: Duration = Duration::from_secs(10);

/// The devices as the relay publishes them.
pub fn to_json(devices: &[Device]) -> String {
    Value::Array(devices.iter().map(Device::to_json).collect()).to_string()
}

/// Reads what a relay answered; entries it cannot read are skipped.
pub fn from_json(body: &[u8]) -> Result<Vec<Device>> {
    let value: Value = serde_json::from_slice(body)?;
    let list = value
        .as_array()
        .ok_or_else(|| fail!("the relay did not answer with a list of devices"))?;
    Ok(list.iter().filter_map(Device::from_json).collect())
}

/// Where a relay publishes its devices, from what was given: `host`, `host:port` or a URL.
pub fn devices_url(relay: &str) -> String {
    let relay = relay.trim().trim_end_matches('/');
    let base = if relay.starts_with("http://") {
        relay.to_string()
    } else if relay.contains(':') {
        format!("http://{relay}")
    } else {
        format!("http://{relay}:{PORT}")
    };
    if base.ends_with("/devices") {
        base
    } else {
        format!("{base}/devices")
    }
}

/// Asks a relay for the screens it sees.
pub fn ask(relay: &str, wait: Duration) -> Result<Vec<Device>> {
    let url = devices_url(relay);
    // The relay searches for `wait` before answering.
    let response = http::request("GET", &url, &[], &[], wait + Duration::from_secs(8))
        .map_err(|error| fail!("cannot reach the relay at {url}: {error}"))?;
    if response.status != 200 {
        return Err(fail!(
            "the relay at {url} answered HTTP {}",
            response.status
        ));
    }
    from_json(&response.body)
}

type Cache = Mutex<Option<(Instant, String)>>;

/// Serves the list of screens until the program is stopped.
pub fn serve(listen: SocketAddr, wait: Duration) -> Result<()> {
    let listener =
        TcpListener::bind(listen).map_err(|error| fail!("cannot listen on {listen}: {error}"))?;
    eprintln!("dapple relay listening on http://{listen}/devices");
    let cache: Arc<Cache> = Arc::new(Mutex::new(None));
    for stream in listener.incoming().flatten() {
        let cache = Arc::clone(&cache);
        thread::spawn(move || {
            let _ = answer(stream, &cache, wait);
        });
    }
    Ok(())
}

fn answer(mut stream: TcpStream, cache: &Cache, wait: Duration) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let head = server::read_head(&mut stream)?;
    let request = server::parse_request(&head);
    let (status, body) = match request
        .as_ref()
        .map(|request| (request.method.as_str(), request.path.as_str()))
    {
        Some(("GET", "/devices")) => ("200 OK", devices(cache, wait)),
        Some(("GET", "/health")) => ("200 OK", "{}".to_string()),
        _ => ("404 Not Found", "{}".to_string()),
    };
    write!(
        stream,
        "HTTP/1.1 {status}\r\nServer: dapple\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    )
}

/// One search at a time: whoever asks during a search gets its result.
fn devices(cache: &Cache, wait: Duration) -> String {
    let mut cached = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((at, body)) = cached.as_ref() {
        if at.elapsed() < FRESH {
            return body.clone();
        }
    }
    let body = to_json(&discovery::discover(wait));
    *cached = Some((Instant::now(), body.clone()));
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_devices_url() {
        assert_eq!(
            devices_url("172.31.250.1"),
            "http://172.31.250.1:8691/devices"
        );
        assert_eq!(devices_url("host:9000"), "http://host:9000/devices");
        assert_eq!(
            devices_url(" http://host:9000/ "),
            "http://host:9000/devices"
        );
        assert_eq!(
            devices_url("http://host:9000/devices"),
            "http://host:9000/devices"
        );
    }

    #[test]
    fn devices_survive_the_trip() {
        let cast = discovery::from_locator("chromecast://192.168.1.42")
            .unwrap()
            .unwrap();
        let listed = from_json(to_json(std::slice::from_ref(&cast)).as_bytes()).unwrap();
        assert_eq!(listed, vec![cast]);
        assert_eq!(from_json(b"[{\"name\":\"broken\"}, 3]").unwrap(), vec![]);
        assert!(from_json(b"{}").is_err());
        assert!(from_json(b"nope").is_err());
    }

    #[test]
    fn answers_only_what_it_is_for() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let cache: Arc<Cache> = Arc::new(Mutex::new(Some((Instant::now(), "[]".to_string()))));
        thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = answer(stream, &cache, Duration::from_millis(10));
            }
        });
        let get = |path: &str| {
            http::request(
                "GET",
                &format!("http://{address}{path}"),
                &[],
                &[],
                Duration::from_secs(5),
            )
            .unwrap()
        };
        assert_eq!(
            (get("/devices").status, get("/devices").body),
            (200, b"[]".to_vec())
        );
        assert_eq!(get("/health").status, 200);
        assert_eq!(get("/").status, 404);
        assert_eq!(get("/devices/extra").status, 404);
        assert_eq!(
            ask(&address.to_string(), Duration::from_millis(10)).unwrap(),
            vec![]
        );
        let post = http::request(
            "POST",
            &format!("http://{address}/devices"),
            &[],
            &[],
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(post.status, 404);
    }
}
