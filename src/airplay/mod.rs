//! Drives an AirPlay screen (Apple TV, AirPlay 2 televisions): pair once with the code shown
//! on the screen, then open an encrypted session and hand the screen a URL to play.
//!
//! Since tvOS 26 a video is started by inserting an item in the device's play queue through
//! `/command`; the older `/play` request is accepted and then ignored.

pub mod bplist;
pub mod channel;
pub mod crypto;
pub mod hap;
pub mod srp;
pub mod store;
pub mod tlv8;

use std::net::{SocketAddr, UdpSocket};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use bplist::{dict, text, Value};
use channel::{compose, Cipher, Connection, Message};

use crate::error::Result;
use crate::fail;
use crate::renderer::{Media, Renderer, State, Status};

const TIMEOUT: Duration = Duration::from_secs(10);
const FEEDBACK_EVERY: Duration = Duration::from_secs(2);
const PLIST: &str = "application/x-apple-binary-plist";
/// The kind of stream that carries a play queue, and the client type Apple's senders name.
const QUEUE_STREAM: i64 = 130;
const CLIENT_TYPE: &str = "A6B27562-B43A-4F2D-B75F-82391E250194";

/// A pairing in progress: the screen shows its code and waits for it on this connection.
pub struct Pairing {
    connection: Connection,
    salt: Vec<u8>,
    public: Vec<u8>,
}

impl Pairing {
    /// Makes the screen show its code.
    pub fn begin(address: SocketAddr) -> Result<Pairing> {
        let mut connection = Connection::open(address, TIMEOUT)?;
        hap::show_code(&mut connection)?;
        let (salt, public) = hap::begin_setup(&mut connection)?;
        Ok(Pairing {
            connection,
            salt,
            public,
        })
    }

    /// Completes the pairing with the code read on the screen.
    pub fn finish(mut self, code: &str) -> Result<hap::Credentials> {
        hap::finish_setup(&mut self.connection, code.trim(), &self.salt, &self.public)
    }
}

/// What the screen last reported about playback.
#[derive(Debug, PartialEq)]
pub struct Playback {
    pub state: String,
    /// Position in seconds when `at` was taken.
    pub position: f64,
    pub duration: f64,
    pub rate: f64,
    pub at: Instant,
    /// The session ended on the device's side.
    pub closed: bool,
}

/// A time as AVFoundation writes it: a value over a timescale.
fn seconds(time: &Value) -> Option<f64> {
    if let Some(plain) = time.as_f64() {
        return Some(plain);
    }
    let scale = time.get("timescale")?.as_f64()?;
    (scale > 0.0).then(|| {
        time.get("value")
            .and_then(Value::as_f64)
            .map(|value| value / scale)
    })?
}

/// Seconds as a time a device accepts as start position.
fn media_time(position: f64) -> Value {
    dict(vec![
        ("value", Value::Int((position * 1000.0).round() as i64)),
        ("timescale", Value::Int(1000)),
        ("flags", Value::Int(1)),
        ("epoch", Value::Int(0)),
    ])
}

/// The message inside an event: events wrap their payload in `params.data`.
pub fn payload(body: &[u8]) -> Option<Value> {
    let outer = bplist::decode(body).ok()?;
    match outer
        .get("params")
        .and_then(|params| params.get("data"))
        .and_then(Value::as_data)
    {
        Some(inner) => bplist::decode(inner).ok(),
        None => Some(outer),
    }
}

impl Playback {
    fn new() -> Playback {
        Playback {
            state: "idle".into(),
            position: 0.0,
            duration: 0.0,
            rate: 0.0,
            at: Instant::now(),
            closed: false,
        }
    }

    /// Updates what is known from one event of the device.
    pub fn note(&mut self, event: &Value, now: Instant) {
        let name = event.get("name").and_then(Value::as_str).unwrap_or("");
        match event.get("type").and_then(Value::as_str) {
            Some("playbackState") => {
                let state = event
                    .get("params")
                    .and_then(|params| params.get("playbackState"))
                    .and_then(Value::as_str)
                    .unwrap_or(name);
                // Freeze the estimate where it is before the rate changes.
                self.position = self.estimate(now);
                self.at = now;
                self.state = state.to_string();
                self.rate = if state == "playing" {
                    self.rate.max(1.0)
                } else {
                    0.0
                };
            }
            Some("notification") if name == "rateChanged" || name == "timeJumped" => {}
            _ => return,
        }
        // Several events carry where playback is, how long the media is and how fast it goes.
        let sources = [Some(event), event.get("params"), event.get("item")];
        let field = |key: &str| {
            sources
                .iter()
                .flatten()
                .find_map(|source| source.get(key))
                .and_then(seconds)
        };
        if let Some(position) = field("position") {
            self.position = position;
            self.at = now;
        }
        if let Some(duration) = field("duration").filter(|duration| *duration > 0.0) {
            self.duration = duration;
        }
        if let Some(rate) = field("rate") {
            self.position = self.estimate(now);
            self.at = now;
            self.rate = rate;
        }
    }

    /// Where playback is now, from the last report and the time since.
    pub fn estimate(&self, now: Instant) -> f64 {
        let elapsed = now.saturating_duration_since(self.at).as_secs_f64();
        let position = self.position + elapsed * self.rate;
        if self.duration > 0.0 {
            position.min(self.duration)
        } else {
            position
        }
    }

    pub fn as_state(&self) -> State {
        match self.state.as_str() {
            "playing" => State::Playing,
            "paused" => State::Paused,
            "loading" | "buffering" | "waiting" => State::Buffering,
            _ => State::Idle,
        }
    }
}

pub struct AirPlay {
    control: Connection,
    uri: String,
    sequence: u32,
    instance: String,
    remote: u32,
    session: String,
    stream: String,
    playback: Arc<Mutex<Playback>>,
    last_feedback: Instant,
    item: Option<String>,
    media: Option<Media>,
    /// Kept open: the session names this port as its timing port.
    _timing: UdpSocket,
}

fn locked(playback: &Mutex<Playback>) -> std::sync::MutexGuard<'_, Playback> {
    playback
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Answers the device's events and keeps what they say about playback.
fn listen(mut events: Connection, playback: Arc<Mutex<Playback>>) {
    loop {
        let request = match events.receive(Duration::from_millis(500)) {
            Ok(Some(request)) => request,
            Ok(None) => continue,
            Err(_) => {
                locked(&playback).closed = true;
                return;
            }
        };
        let mut headers = vec![("Audio-Latency", "0".to_string())];
        for name in ["Server", "CSeq"] {
            if let Some(value) = request.header(name) {
                headers.push((name, value.to_string()));
            }
        }
        if events
            .send(&compose("RTSP/1.0 200 OK", &headers, &[]))
            .is_err()
        {
            locked(&playback).closed = true;
            return;
        }
        if let Some(event) = payload(&request.body) {
            crate::debug::log(|| format!("[airplay] event {}", summary(&event)));
            locked(&playback).note(&event, Instant::now());
        }
    }
}

/// A one-line view of a message, for traces: keys and short values only.
pub fn summary(value: &Value) -> String {
    match value {
        Value::Dict(entries) => {
            let parts: Vec<String> = entries
                .iter()
                .map(|(key, value)| format!("{key}={}", summary(value)))
                .collect();
            format!("{{{}}}", parts.join(" "))
        }
        Value::Array(items) => format!("[{} items]", items.len()),
        Value::Data(bytes) => format!("<{} bytes>", bytes.len()),
        Value::Str(text) if text.len() > 60 => {
            format!("\"{}…\"", text.chars().take(60).collect::<String>())
        }
        Value::Str(text) => format!("\"{text}\""),
        Value::Int(number) => number.to_string(),
        Value::Real(number) => number.to_string(),
        Value::Bool(flag) => flag.to_string(),
        Value::Null => "null".to_string(),
    }
}

impl AirPlay {
    /// Opens an encrypted session with a paired device and prepares it to receive a play queue.
    pub fn open(address: SocketAddr, credentials: &hap::Credentials) -> Result<AirPlay> {
        let mut control = Connection::open(address, TIMEOUT)?;
        let shared = hap::verify(&mut control, credentials)?;
        control.encrypt(Cipher::new(
            crypto::derive(&shared, "Control-Salt", "Control-Write-Encryption-Key")?,
            crypto::derive(&shared, "Control-Salt", "Control-Read-Encryption-Key")?,
        ));

        let timing = UdpSocket::bind("0.0.0.0:0")?;
        let session_number = u32::from_le_bytes(crypto::random()?);
        let mut airplay = AirPlay {
            uri: format!("rtsp://{}/{session_number}", control.local_ip()?),
            control,
            sequence: 0,
            instance: crypto::hex(&crypto::random::<8>()?).to_uppercase(),
            remote: u32::from_le_bytes(crypto::random()?),
            session: crypto::uuid()?,
            stream: "1".to_string(),
            playback: Arc::new(Mutex::new(Playback::new())),
            last_feedback: Instant::now(),
            item: None,
            media: None,
            _timing: timing,
        };

        let timing_port = i64::from(airplay._timing.local_addr()?.port());
        let setup = airplay.rtsp(
            "SETUP",
            None,
            Some(&dict(vec![
                ("deviceID", text("AA:BB:CC:DD:EE:FF")),
                ("sessionUUID", text(&airplay.session.clone())),
                ("sessionCorrelationUUID", text(&crypto::uuid()?)),
                ("timingPort", Value::Int(timing_port)),
                ("timingProtocol", text("NTP")),
                ("isMultiSelectAirPlay", Value::Bool(true)),
                ("groupContainsGroupLeader", Value::Bool(false)),
                ("macAddress", text("AA:BB:CC:DD:EE:FF")),
                ("model", text("iPhone14,3")),
                ("name", text("dapple")),
                ("osBuildVersion", text("20F66")),
                ("osName", text("iPhone OS")),
                ("osVersion", text("16.5")),
                ("senderSupportsRelay", Value::Bool(false)),
                ("sourceVersion", text("690.7.1")),
                ("statsCollectionEnabled", Value::Bool(false)),
            ])),
        )?;
        let event_port = bplist::decode(&setup.body)
            .ok()
            .and_then(|answer| answer.get("eventPort").and_then(Value::as_i64))
            .filter(|port| (1..=65535).contains(port))
            .ok_or_else(|| fail!("the device did not open its event channel"))?;

        // The device opens the port a moment after naming it.
        let event_address = SocketAddr::new(address.ip(), event_port as u16);
        let mut events = None;
        for _ in 0..5 {
            match Connection::open(event_address, TIMEOUT) {
                Ok(connection) => {
                    events = Some(connection);
                    break;
                }
                Err(_) => thread::sleep(Duration::from_millis(500)),
            }
        }
        let mut events = events.ok_or_else(|| fail!("cannot reach the device's event channel"))?;
        // Named from the device's side: it writes what we read.
        events.encrypt(Cipher::new(
            crypto::derive(&shared, "Events-Salt", "Events-Read-Encryption-Key")?,
            crypto::derive(&shared, "Events-Salt", "Events-Write-Encryption-Key")?,
        ));
        let playback = Arc::clone(&airplay.playback);
        thread::spawn(move || listen(events, playback));

        airplay.rtsp("GET", Some("/info"), None)?;
        airplay.rtsp("RECORD", None, None)?;
        let stream = airplay.rtsp(
            "SETUP",
            None,
            Some(&dict(vec![(
                "streams",
                Value::Array(vec![dict(vec![
                    ("clientUUID", text(&crypto::uuid()?)),
                    ("clientTypeUUID", text(CLIENT_TYPE)),
                    ("channelID", text("36:CB:3F:E1:93:B0-RCS-1")),
                    ("controlType", Value::Int(1)),
                    ("type", Value::Int(QUEUE_STREAM)),
                ])]),
            )])),
        )?;
        if let Some(id) = bplist::decode(&stream.body).ok().and_then(|answer| {
            answer
                .get("streams")?
                .as_array()?
                .first()?
                .get("streamID")?
                .as_i64()
        }) {
            airplay.stream = id.to_string();
        }
        Ok(airplay)
    }

    /// One RTSP request on the control channel; an error status is an error.
    fn rtsp(&mut self, method: &str, path: Option<&str>, body: Option<&Value>) -> Result<Message> {
        let sequence = self.sequence;
        self.sequence += 1;
        let mut headers = vec![
            ("CSeq", sequence.to_string()),
            ("DACP-ID", self.instance.clone()),
            ("Active-Remote", self.remote.to_string()),
            ("Client-Instance", self.instance.clone()),
            ("User-Agent", "AirPlay/550.10".to_string()),
        ];
        let encoded = body.map(bplist::encode).unwrap_or_default();
        if body.is_some() {
            headers.push(("Content-Type", PLIST.to_string()));
        }
        let target = path.unwrap_or(&self.uri).to_string();
        let request = compose(&format!("{method} {target} RTSP/1.0"), &headers, &encoded);
        let response = self.control.exchange(&request, TIMEOUT)?;
        crate::debug::log(|| format!("[airplay] {method} {target} -> {}", response.status()));
        if response.status() >= 400 {
            return Err(fail!(
                "the device refused {method} {target} (status {})",
                response.status()
            ));
        }
        Ok(response)
    }

    /// One play-queue command, wrapped as the device expects.
    fn command(&mut self, data: Value) -> Result<()> {
        let kind = data
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or("command")
            .to_string();
        let body = bplist::encode(&dict(vec![(
            "params",
            dict(vec![("data", Value::Data(bplist::encode(&data)))]),
        )]));
        let request = compose(
            "POST /command HTTP/1.1",
            &[
                ("User-Agent", "AirPlay/870.14.1".to_string()),
                ("Content-Type", PLIST.to_string()),
                ("X-Apple-ProtocolVersion", "1".to_string()),
                ("X-Apple-Session-ID", self.session.clone()),
                ("X-Apple-StreamID", self.stream.clone()),
            ],
            &body,
        );
        let response = self.control.exchange(&request, TIMEOUT)?;
        crate::debug::log(|| format!("[airplay] command {kind} -> {}", response.status()));
        if response.status() >= 400 {
            return Err(fail!(
                "the device refused {kind} (status {})",
                response.status()
            ));
        }
        Ok(())
    }

    /// Tells the device the sender is still there; it ends silent sessions.
    fn keep_alive(&mut self) -> Result<()> {
        if self.last_feedback.elapsed() >= FEEDBACK_EVERY {
            self.last_feedback = Instant::now();
            self.rtsp("POST", Some("/feedback"), None)?;
        }
        Ok(())
    }

    fn rate(&mut self, rate: f64) -> Result<()> {
        self.command(dict(vec![
            ("type", text("setRate")),
            ("rate", Value::Real(rate)),
        ]))
    }

    fn queue(&mut self, media: &Media, start: f64) -> Result<()> {
        let item = crypto::uuid()?;
        let mut entry = vec![
            ("uuid", text(&item)),
            ("mediaType", text("file")),
            ("Content-Location", text(&media.url)),
        ];
        if start > 0.0 {
            entry.push(("Start-Position", media_time(start)));
        }
        {
            let mut playback = locked(&self.playback);
            playback.state = "loading".to_string();
            playback.position = start;
            playback.rate = 0.0;
            playback.at = Instant::now();
            if let Some(duration) = media.duration {
                playback.duration = duration;
            }
        }
        self.command(dict(vec![
            ("type", text("insertPlayQueueItem")),
            ("item", dict(entry)),
        ]))?;
        self.command(dict(vec![
            ("type", text("setProperty")),
            ("value", Value::Bool(true)),
            ("property", text("isInterestedInDateRange")),
            ("item", dict(vec![("uuid", text(&item))])),
        ]))?;
        self.command(dict(vec![
            ("type", text("setProperty")),
            ("value", Value::Int(1)),
            ("property", text("actionAtItemEnd")),
        ]))?;
        self.rate(1.0)?;
        self.item = Some(item);
        Ok(())
    }
}

impl Renderer for AirPlay {
    fn load(&mut self, media: &Media) -> Result<()> {
        self.queue(media, if media.seekable { media.start } else { 0.0 })?;
        self.media = Some(media.clone());
        // The device reports "loading", then "playing" once it has fetched enough.
        let deadline = Instant::now() + Duration::from_secs(40);
        loop {
            {
                let playback = locked(&self.playback);
                match playback.as_state() {
                    State::Playing | State::Paused => return Ok(()),
                    _ if playback.closed => return Err(fail!("the device ended the session")),
                    State::Idle if playback.state != "idle" => {
                        return Err(fail!(
                            "the device cannot play this media ({})",
                            playback.state
                        ))
                    }
                    _ => {}
                }
            }
            if Instant::now() >= deadline {
                return Err(fail!("the device did not start playing"));
            }
            self.keep_alive()?;
            thread::sleep(Duration::from_millis(250));
        }
    }

    fn play(&mut self) -> Result<()> {
        self.rate(1.0)
    }

    fn pause(&mut self) -> Result<()> {
        self.rate(0.0)
    }

    fn stop(&mut self) -> Result<()> {
        // The device keeps its player open until the sender leaves: pause, then end the session.
        let _ = self.rate(0.0);
        self.rtsp("TEARDOWN", None, None).map(drop)
    }

    fn seek(&mut self, seconds: f64) -> Result<()> {
        // The queue item is replaced by the same media starting where asked.
        let media = self
            .media
            .clone()
            .ok_or_else(|| fail!("nothing is playing on this device"))?;
        self.queue(&media, seconds.max(0.0))
    }

    fn set_volume(&mut self, _percent: u8) -> Result<()> {
        Err(fail!(
            "use the TV's remote for the volume: AirPlay screens do not take it from here yet"
        ))
    }

    fn status(&mut self) -> Result<Status> {
        self.keep_alive()?;
        let playback = locked(&self.playback);
        if playback.closed {
            return Ok(Status {
                state: State::Idle,
                position: playback.position,
                duration: playback.duration,
                volume: None,
            });
        }
        Ok(Status {
            state: playback.as_state(),
            position: playback.estimate(Instant::now()),
            duration: playback.duration,
            volume: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(entries: Vec<(&str, Value)>) -> Value {
        dict(entries)
    }

    #[test]
    fn reads_times() {
        assert_eq!(seconds(&Value::Real(2.5)), Some(2.5));
        assert_eq!(seconds(&Value::Int(4)), Some(4.0));
        assert_eq!(
            seconds(&dict(vec![
                ("value", Value::Int(4500)),
                ("timescale", Value::Int(1000))
            ])),
            Some(4.5)
        );
        assert_eq!(
            seconds(&dict(vec![
                ("value", Value::Int(1)),
                ("timescale", Value::Int(0))
            ])),
            None
        );
        assert_eq!(seconds(&text("soon")), None);
        let time = media_time(12.3456);
        assert_eq!(seconds(&time), Some(12.346));
        assert_eq!(time.get("flags"), Some(&Value::Int(1)));
    }

    #[test]
    fn unwraps_event_payloads() {
        let inner = event(vec![
            ("type", text("playbackState")),
            ("name", text("playing")),
        ]);
        let wrapped = bplist::encode(&dict(vec![(
            "params",
            dict(vec![("data", Value::Data(bplist::encode(&inner)))]),
        )]));
        assert_eq!(payload(&wrapped), Some(inner));
        let plain = event(vec![("type", text("updateInfo"))]);
        assert_eq!(payload(&bplist::encode(&plain)), Some(plain));
        assert_eq!(payload(b"nope"), None);
    }

    #[test]
    fn follows_playback() {
        let start = Instant::now();
        let mut playback = Playback::new();
        assert_eq!(playback.as_state(), State::Idle);

        playback.note(
            &event(vec![
                ("type", text("playbackState")),
                ("name", text("loading")),
            ]),
            start,
        );
        assert_eq!(playback.as_state(), State::Buffering);
        assert_eq!(playback.estimate(start + Duration::from_secs(5)), 0.0);

        playback.note(
            &event(vec![
                ("type", text("playbackState")),
                ("name", text("playing")),
                (
                    "position",
                    dict(vec![
                        ("value", Value::Int(10_000)),
                        ("timescale", Value::Int(1000)),
                    ]),
                ),
                ("duration", Value::Real(600.0)),
                ("rate", Value::Real(1.0)),
            ]),
            start,
        );
        assert_eq!(playback.as_state(), State::Playing);
        assert_eq!(playback.estimate(start + Duration::from_secs(5)), 15.0);
        assert_eq!(playback.estimate(start + Duration::from_secs(5000)), 600.0);

        let later = start + Duration::from_secs(20);
        playback.note(
            &event(vec![
                ("type", text("playbackState")),
                ("params", dict(vec![("playbackState", text("paused"))])),
            ]),
            later,
        );
        assert_eq!(playback.as_state(), State::Paused);
        assert_eq!(playback.estimate(later + Duration::from_secs(60)), 30.0);

        playback.note(
            &event(vec![
                ("type", text("notification")),
                ("name", text("timeJumped")),
                ("position", Value::Real(100.0)),
            ]),
            later,
        );
        assert_eq!(playback.estimate(later), 100.0);
        playback.note(
            &event(vec![
                ("type", text("notification")),
                ("name", text("loadedTimeRangesChanged")),
                ("position", Value::Real(1.0)),
            ]),
            later,
        );
        assert_eq!(playback.estimate(later), 100.0);
        playback.note(&event(vec![("type", text("updateInfo"))]), later);

        playback.note(
            &event(vec![
                ("type", text("playbackState")),
                ("name", text("stopped")),
            ]),
            later,
        );
        assert_eq!(playback.as_state(), State::Idle);
    }

    #[test]
    fn summarizes_messages() {
        let value = dict(vec![
            ("type", text("x")),
            ("data", Value::Data(vec![0; 9])),
            ("list", Value::Array(vec![Value::Null])),
            ("n", Value::Int(3)),
            ("r", Value::Real(1.5)),
            ("b", Value::Bool(true)),
            ("none", Value::Null),
            ("long", text(&"a".repeat(80))),
        ]);
        let line = summary(&value);
        assert!(line.starts_with(
            "{type=\"x\" data=<9 bytes> list=[1 items] n=3 r=1.5 b=true none=null long=\""
        ));
        assert!(line.ends_with("…\"}"));
    }
}
