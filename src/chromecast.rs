//! Drives a Google Cast device: JSON messages wrapped in a small protobuf envelope, over TLS.

use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::sync::Arc;
use std::time::{Duration, Instant};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};
use serde_json::{json, Value};

use crate::error::Result;
use crate::fail;
use crate::renderer::{Media, Renderer, State, Status};

const SENDER: &str = "sender-0";
const RECEIVER: &str = "receiver-0";
const NS_CONNECTION: &str = "urn:x-cast:com.google.cast.tp.connection";
const NS_HEARTBEAT: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
const NS_RECEIVER: &str = "urn:x-cast:com.google.cast.receiver";
const NS_MEDIA: &str = "urn:x-cast:com.google.cast.media";

/// Google's Default Media Receiver: the player every Cast device can launch.
const MEDIA_RECEIVER: &str = "CC1AD845";

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Short, so that waiting for one message never blocks a command for long.
const READ_SLICE: Duration = Duration::from_millis(250);
const PING_EVERY: Duration = Duration::from_secs(4);
/// No frame comes near this size; a larger length means the stream is out of step.
const MAX_FRAME: usize = 1 << 20;

/// One Cast message, out of its envelope.
#[derive(Debug, PartialEq)]
pub struct Frame {
    pub source: String,
    pub destination: String,
    pub namespace: String,
    pub payload: String,
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push(value as u8 | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn put_string(out: &mut Vec<u8>, field: u8, text: &str) {
    out.push(field << 3 | 2);
    put_varint(out, text.len() as u64);
    out.extend_from_slice(text.as_bytes());
}

/// Wraps a message in its `CastMessage` envelope, prefixed by its length.
pub fn encode(frame: &Frame) -> Vec<u8> {
    // Field 1: protocol version 0. Field 5: payload type 0 (text).
    let mut body = vec![0x08, 0x00];
    put_string(&mut body, 2, &frame.source);
    put_string(&mut body, 3, &frame.destination);
    put_string(&mut body, 4, &frame.namespace);
    body.extend_from_slice(&[0x28, 0x00]);
    put_string(&mut body, 6, &frame.payload);
    let mut framed = (body.len() as u32).to_be_bytes().to_vec();
    framed.extend_from_slice(&body);
    framed
}

fn take_varint(data: &mut &[u8]) -> Option<u64> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let (&byte, rest) = data.split_first()?;
        *data = rest;
        value |= u64::from(byte & 0x7F) << shift;
        if byte & 0x80 == 0 {
            return Some(value);
        }
    }
    None
}

/// Reads a `CastMessage` envelope (without its length prefix).
pub fn decode(mut data: &[u8]) -> Option<Frame> {
    let mut frame = Frame {
        source: String::new(),
        destination: String::new(),
        namespace: String::new(),
        payload: String::new(),
    };
    while !data.is_empty() {
        let key = take_varint(&mut data)?;
        match key & 7 {
            0 => {
                take_varint(&mut data)?;
            }
            2 => {
                let length = take_varint(&mut data)? as usize;
                let bytes = data.get(..length)?;
                data = &data[length..];
                let text = String::from_utf8_lossy(bytes).into_owned();
                match key >> 3 {
                    2 => frame.source = text,
                    3 => frame.destination = text,
                    4 => frame.namespace = text,
                    6 => frame.payload = text,
                    _ => {}
                }
            }
            1 => data = data.get(8..)?,
            5 => data = data.get(4..)?,
            _ => return None,
        }
    }
    Some(frame)
}

/// Takes one complete frame off the front of the received bytes.
pub fn take_frame(buffer: &mut Vec<u8>) -> Result<Option<Vec<u8>>> {
    let Some(prefix) = buffer.get(..4) else {
        return Ok(None);
    };
    let length = u32::from_be_bytes([prefix[0], prefix[1], prefix[2], prefix[3]]) as usize;
    if length > MAX_FRAME {
        return Err(fail!("the device sent an unreadable message"));
    }
    if buffer.len() < 4 + length {
        return Ok(None);
    }
    let frame = buffer[4..4 + length].to_vec();
    buffer.drain(..4 + length);
    Ok(Some(frame))
}

/// Cast devices present a certificate they signed themselves, for an address that is not in
/// it: there is nothing to check it against. Like every Cast sender on a home network, the
/// connection is encrypted but the device is not authenticated.
#[derive(Debug)]
struct AcceptDeviceCertificate;

impl ServerCertVerifier for AcceptDeviceCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// What the device last said about itself.
#[derive(Debug, Default, PartialEq)]
pub struct Seen {
    /// Where the running application listens, when it can play media.
    pub media_transport: Option<String>,
    pub app: Option<String>,
    pub volume: Option<f64>,
    pub media_session: Option<i64>,
    pub player: String,
    pub time: f64,
    pub duration: f64,
    /// The type of the last refusal (`LOAD_FAILED`…).
    pub refusal: Option<String>,
    /// The request the last media status answered.
    pub answered: i64,
}

impl Seen {
    /// Updates what is known from one message.
    pub fn note(&mut self, message: &Value) {
        match message["type"].as_str().unwrap_or("") {
            "RECEIVER_STATUS" => {
                let status = &message["status"];
                if let Some(level) = status["volume"]["level"].as_f64() {
                    self.volume = Some(level);
                }
                let media_app = status["applications"].as_array().and_then(|applications| {
                    applications.iter().find(|application| {
                        application["namespaces"].as_array().is_some_and(|spaces| {
                            spaces.iter().any(|space| space["name"] == NS_MEDIA)
                        })
                    })
                });
                self.app = media_app
                    .and_then(|app| app["appId"].as_str())
                    .map(str::to_string);
                self.media_transport = media_app
                    .and_then(|app| app["transportId"].as_str())
                    .map(str::to_string);
            }
            "MEDIA_STATUS" => {
                self.answered = message["requestId"].as_i64().unwrap_or(0);
                match message["status"].get(0) {
                    Some(status) => {
                        self.media_session = status["mediaSessionId"].as_i64();
                        self.player = status["playerState"].as_str().unwrap_or("IDLE").to_string();
                        self.time = status["currentTime"].as_f64().unwrap_or(self.time);
                        // The duration comes only with the statuses that describe the media.
                        if let Some(duration) = status["media"]["duration"].as_f64() {
                            self.duration = duration;
                        }
                    }
                    None => {
                        self.media_session = None;
                        self.player = "IDLE".to_string();
                    }
                }
            }
            refusal @ ("LOAD_FAILED" | "LOAD_CANCELLED" | "INVALID_REQUEST" | "LAUNCH_ERROR") => {
                self.refusal = Some(refusal.to_string());
            }
            _ => {}
        }
    }

    pub fn state(&self) -> State {
        match self.player.as_str() {
            "PLAYING" => State::Playing,
            "PAUSED" => State::Paused,
            "BUFFERING" => State::Buffering,
            _ => State::Idle,
        }
    }
}

/// The `LOAD` request for a media.
pub fn load_request(media: &Media, request: i64) -> Value {
    let mut description = json!({
        "contentId": media.url,
        "contentType": media.mime,
        "streamType": "BUFFERED",
        "metadata": { "metadataType": 0, "title": media.title },
    });
    if let Some(duration) = media.duration.filter(|_| !media.seekable) {
        // A converted stream has no length of its own: tell the player what to expect.
        description["duration"] = json!(duration);
    }
    let mut request = json!({
        "type": "LOAD",
        "requestId": request,
        "autoplay": true,
        "currentTime": if media.seekable { media.start } else { 0.0 },
    });
    if let Some(subtitles) = &media.subtitles_vtt {
        description["tracks"] = json!([{
            "trackId": 1,
            "type": "TEXT",
            "subtype": "SUBTITLES",
            "trackContentId": subtitles,
            "trackContentType": "text/vtt",
            "name": "Subtitles",
        }]);
        request["activeTrackIds"] = json!([1]);
    }
    request["media"] = description;
    request
}

pub struct Chromecast {
    stream: StreamOwned<ClientConnection, TcpStream>,
    received: Vec<u8>,
    seen: Seen,
    request: i64,
    /// The application transport this connection has joined.
    joined: Option<String>,
    last_ping: Instant,
}

impl Chromecast {
    pub fn open(address: SocketAddr) -> Result<Chromecast> {
        let config =
            ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
                .with_safe_default_protocol_versions()?
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(AcceptDeviceCertificate))
                .with_no_client_auth();
        let mut tcp = TcpStream::connect_timeout(&address, CONNECT_TIMEOUT)
            .map_err(|error| fail!("cannot reach the device at {address}: {error}"))?;
        tcp.set_nodelay(true)?;
        tcp.set_read_timeout(Some(CONNECT_TIMEOUT))?;
        tcp.set_write_timeout(Some(CONNECT_TIMEOUT))?;
        let mut connection =
            ClientConnection::new(Arc::new(config), ServerName::IpAddress(address.ip().into()))
                .map_err(|error| fail!("cannot start a secure connection: {error}"))?;
        while connection.is_handshaking() {
            connection
                .complete_io(&mut tcp)
                .map_err(|error| fail!("the device at {address} does not speak Cast: {error}"))?;
        }
        tcp.set_read_timeout(Some(READ_SLICE))?;
        let mut cast = Chromecast {
            stream: StreamOwned::new(connection, tcp),
            received: Vec::new(),
            seen: Seen::default(),
            request: 0,
            joined: None,
            last_ping: Instant::now(),
        };
        cast.send(RECEIVER, NS_CONNECTION, &json!({ "type": "CONNECT" }))?;
        Ok(cast)
    }

    fn send(&mut self, destination: &str, namespace: &str, payload: &Value) -> Result<()> {
        let frame = Frame {
            source: SENDER.to_string(),
            destination: destination.to_string(),
            namespace: namespace.to_string(),
            payload: payload.to_string(),
        };
        self.stream.write_all(&encode(&frame))?;
        self.stream.flush()?;
        Ok(())
    }

    fn next_request(&mut self) -> i64 {
        self.request += 1;
        self.request
    }

    /// Reads one message if one arrives within a short slice of time.
    fn read(&mut self) -> Result<Option<Frame>> {
        loop {
            if let Some(frame) = take_frame(&mut self.received)? {
                return Ok(decode(&frame));
            }
            let mut chunk = [0u8; 4096];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Err(fail!("the device closed the connection")),
                Ok(size) => self.received.extend_from_slice(&chunk[..size]),
                Err(error)
                    if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) =>
                {
                    return Ok(None)
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    /// Handles incoming messages until `done` holds or time runs out; tells which.
    fn wait(&mut self, limit: Duration, done: impl Fn(&Seen) -> bool) -> Result<bool> {
        let deadline = Instant::now() + limit;
        loop {
            if done(&self.seen) {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            let Some(frame) = self.read()? else {
                continue;
            };
            let Ok(message) = serde_json::from_str::<Value>(&frame.payload) else {
                continue;
            };
            match message["type"].as_str() {
                Some("PING") => {
                    self.send(&frame.source, NS_HEARTBEAT, &json!({ "type": "PONG" }))?
                }
                Some("CLOSE") if self.joined.as_deref() == Some(frame.source.as_str()) => {
                    // The application stopped (another sender took over, or the TV was used).
                    self.joined = None;
                    self.seen.media_transport = None;
                    self.seen.media_session = None;
                    self.seen.player = "IDLE".to_string();
                }
                _ => self.seen.note(&message),
            }
        }
    }

    fn keep_alive(&mut self) -> Result<()> {
        if self.last_ping.elapsed() >= PING_EVERY {
            self.last_ping = Instant::now();
            self.send(RECEIVER, NS_HEARTBEAT, &json!({ "type": "PING" }))?;
        }
        Ok(())
    }

    /// Joins the media application if one runs; tells whether it does.
    fn join(&mut self) -> Result<bool> {
        if self.joined.is_some() {
            return Ok(true);
        }
        if self.seen.media_transport.is_none() {
            let request = self.next_request();
            self.send(
                RECEIVER,
                NS_RECEIVER,
                &json!({ "type": "GET_STATUS", "requestId": request }),
            )?;
            self.wait(Duration::from_secs(3), |seen| {
                seen.media_transport.is_some()
            })?;
        }
        let Some(transport) = self.seen.media_transport.clone() else {
            return Ok(false);
        };
        self.send(&transport, NS_CONNECTION, &json!({ "type": "CONNECT" }))?;
        self.joined = Some(transport);
        Ok(true)
    }

    /// Makes sure the Default Media Receiver runs and is joined.
    fn launch(&mut self) -> Result<String> {
        if self.joined.is_none() || self.seen.app.as_deref() != Some(MEDIA_RECEIVER) {
            self.joined = None;
            self.seen.refusal = None;
            self.seen.app = None;
            self.seen.media_transport = None;
            let request = self.next_request();
            self.send(
                RECEIVER,
                NS_RECEIVER,
                &json!({ "type": "LAUNCH", "appId": MEDIA_RECEIVER, "requestId": request }),
            )?;
            let launched = self.wait(Duration::from_secs(15), |seen| {
                seen.refusal.is_some() || seen.app.as_deref() == Some(MEDIA_RECEIVER)
            })?;
            if !launched || self.seen.refusal.is_some() {
                return Err(fail!("the device did not open its media player"));
            }
            self.join()?;
        }
        self.joined
            .clone()
            .ok_or_else(|| fail!("the device did not open its media player"))
    }

    /// Sends a command about the media being played.
    fn media_command(&mut self, kind: &str, extra: Value) -> Result<()> {
        if self.seen.media_session.is_none() {
            self.status()?;
        }
        let (Some(transport), Some(session)) = (self.joined.clone(), self.seen.media_session)
        else {
            return Err(fail!("nothing is playing on this device"));
        };
        let mut command = json!({
            "type": kind,
            "requestId": self.next_request(),
            "mediaSessionId": session,
        });
        if let (Some(command), Some(extra)) = (command.as_object_mut(), extra.as_object()) {
            command.extend(extra.clone());
        }
        self.send(&transport, NS_MEDIA, &command)
    }
}

impl Renderer for Chromecast {
    fn load(&mut self, media: &Media) -> Result<()> {
        let transport = self.launch()?;
        self.seen.refusal = None;
        self.seen.media_session = None;
        self.seen.player = "IDLE".to_string();
        let request = self.next_request();
        self.send(&transport, NS_MEDIA, &load_request(media, request))?;
        let answered = self.wait(Duration::from_secs(30), |seen| {
            seen.refusal.is_some() || (seen.media_session.is_some() && seen.state() != State::Idle)
        })?;
        match &self.seen.refusal {
            Some(refusal) => Err(fail!("the device cannot play this media ({refusal})")),
            None if !answered => Err(fail!("the device did not start playing")),
            None => Ok(()),
        }
    }

    fn play(&mut self) -> Result<()> {
        self.media_command("PLAY", Value::Null)
    }

    fn pause(&mut self) -> Result<()> {
        self.media_command("PAUSE", Value::Null)
    }

    fn stop(&mut self) -> Result<()> {
        self.media_command("STOP", Value::Null)
    }

    fn seek(&mut self, seconds: f64) -> Result<()> {
        self.media_command("SEEK", json!({ "currentTime": seconds.max(0.0) }))
    }

    fn set_volume(&mut self, percent: u8) -> Result<()> {
        let level = f64::from(percent.min(100)) / 100.0;
        let request = self.next_request();
        self.send(
            RECEIVER,
            NS_RECEIVER,
            &json!({ "type": "SET_VOLUME", "volume": { "level": level }, "requestId": request }),
        )?;
        self.seen.volume = Some(level);
        Ok(())
    }

    fn status(&mut self) -> Result<Status> {
        self.keep_alive()?;
        if self.join()? {
            let request = self.next_request();
            if let Some(transport) = self.joined.clone() {
                self.send(
                    &transport,
                    NS_MEDIA,
                    &json!({ "type": "GET_STATUS", "requestId": request }),
                )?;
                self.wait(Duration::from_secs(2), |seen| seen.answered == request)?;
            }
        } else {
            // Still drain what the device says (volume changes, pings).
            self.wait(READ_SLICE, |_| false)?;
        }
        Ok(Status {
            state: self.seen.state(),
            position: self.seen.time,
            duration: self.seen.duration,
            volume: self.seen.volume.map(|level| (level * 100.0).round() as u8),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(payload: &str) -> Frame {
        Frame {
            source: SENDER.into(),
            destination: RECEIVER.into(),
            namespace: NS_CONNECTION.into(),
            payload: payload.into(),
        }
    }

    #[test]
    fn envelopes_round_trip() {
        let long = "x".repeat(300);
        for payload in ["{\"type\":\"CONNECT\"}", "", long.as_str()] {
            let bytes = encode(&frame(payload));
            let length = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
            assert_eq!(length, bytes.len() - 4);
            assert_eq!(decode(&bytes[4..]), Some(frame(payload)));
        }
        // The exact bytes of a known message.
        let bytes = encode(&Frame {
            source: "a".into(),
            destination: "b".into(),
            namespace: "n".into(),
            payload: "p".into(),
        });
        assert_eq!(
            bytes,
            [
                0, 0, 0, 16, 0x08, 0, 0x12, 1, b'a', 0x1A, 1, b'b', 0x22, 1, b'n', 0x28, 0, 0x32,
                1, b'p'
            ]
        );
    }

    #[test]
    fn decoding_skips_unknown_fields_and_rejects_garbage() {
        // Field 7 (binary payload), a 64-bit field 9 and a 32-bit field 10 are ignored.
        let mut bytes = vec![
            0x3A, 2, 1, 2, 0x49, 0, 0, 0, 0, 0, 0, 0, 0, 0x55, 0, 0, 0, 0,
        ];
        put_string(&mut bytes, 4, "ns");
        assert_eq!(decode(&bytes).unwrap().namespace, "ns");
        assert_eq!(decode(&[0x12, 5, b'a']), None);
        assert_eq!(decode(&[0x0B]), None);
        assert_eq!(decode(&[0x08]), None);
        assert_eq!(decode(&[0x49, 0]), None);
        assert_eq!(decode(&[0x55, 0]), None);
        assert_eq!(decode(&[0x80; 11]), None);
    }

    #[test]
    fn frames_are_cut_from_the_stream() {
        let first = encode(&frame("one"));
        let second = encode(&frame("two"));
        let mut buffer = first.clone();
        buffer.extend_from_slice(&second[..7]);
        assert_eq!(take_frame(&mut buffer).unwrap(), Some(first[4..].to_vec()));
        assert_eq!(take_frame(&mut buffer).unwrap(), None);
        buffer.extend_from_slice(&second[7..]);
        assert_eq!(take_frame(&mut buffer).unwrap(), Some(second[4..].to_vec()));
        assert!(buffer.is_empty());
        assert_eq!(take_frame(&mut vec![0, 0]).unwrap(), None);
        assert!(take_frame(&mut vec![0xFF, 0, 0, 0, 1]).is_err());
    }

    #[test]
    fn follows_what_the_device_says() {
        let mut seen = Seen::default();
        assert_eq!(seen.state(), State::Idle);
        seen.note(&json!({"type": "RECEIVER_STATUS", "status": {
            "volume": {"level": 0.4},
            "applications": [
                {"appId": "BACKDROP", "transportId": "t-0", "namespaces": [{"name": "urn:x-cast:other"}]},
                {"appId": MEDIA_RECEIVER, "transportId": "t-1", "namespaces": [{"name": NS_MEDIA}]}
            ]}}));
        assert_eq!(seen.volume, Some(0.4));
        assert_eq!(seen.app.as_deref(), Some(MEDIA_RECEIVER));
        assert_eq!(seen.media_transport.as_deref(), Some("t-1"));

        seen.note(&json!({"type": "MEDIA_STATUS", "requestId": 7, "status": [{
            "mediaSessionId": 3, "playerState": "PLAYING", "currentTime": 12.5,
            "media": {"duration": 600.0}}]}));
        assert_eq!(
            (
                seen.media_session,
                seen.state(),
                seen.time,
                seen.duration,
                seen.answered
            ),
            (Some(3), State::Playing, 12.5, 600.0, 7)
        );
        seen.note(&json!({"type": "MEDIA_STATUS", "status": [{"mediaSessionId": 3, "playerState": "PAUSED"}]}));
        assert_eq!(
            (seen.state(), seen.time, seen.duration, seen.answered),
            (State::Paused, 12.5, 600.0, 0)
        );
        seen.note(&json!({"type": "MEDIA_STATUS", "status": [{"mediaSessionId": 3, "playerState": "BUFFERING"}]}));
        assert_eq!(seen.state(), State::Buffering);
        seen.note(&json!({"type": "MEDIA_STATUS", "status": []}));
        assert_eq!((seen.media_session, seen.state()), (None, State::Idle));

        seen.note(&json!({"type": "RECEIVER_STATUS", "status": {"applications": []}}));
        assert_eq!(
            (seen.app.clone(), seen.media_transport.clone(), seen.volume),
            (None, None, Some(0.4))
        );
        seen.note(&json!({"type": "LOAD_FAILED"}));
        assert_eq!(seen.refusal.as_deref(), Some("LOAD_FAILED"));
        seen.note(&json!({"type": "SOMETHING_ELSE"}));
        seen.note(&json!({}));
    }

    #[test]
    fn builds_load_requests() {
        let mut media = Media {
            url: "http://192.168.1.2:4000/t/file.mp4".into(),
            mime: "video/mp4".into(),
            title: "Film".into(),
            start: 90.0,
            duration: Some(600.0),
            seekable: true,
            ..Media::default()
        };
        let direct = load_request(&media, 4);
        assert_eq!(direct["type"], "LOAD");
        assert_eq!(direct["requestId"], 4);
        assert_eq!(direct["currentTime"], 90.0);
        assert_eq!(
            direct["media"]["contentId"],
            "http://192.168.1.2:4000/t/file.mp4"
        );
        assert_eq!(direct["media"]["metadata"]["title"], "Film");
        assert!(
            direct["media"].get("duration").is_none() && direct.get("activeTrackIds").is_none()
        );

        media.seekable = false;
        media.subtitles_vtt = Some("http://192.168.1.2:4000/t/subtitles.vtt".into());
        let converted = load_request(&media, 5);
        assert_eq!(converted["currentTime"], 0.0);
        assert_eq!(converted["media"]["duration"], 600.0);
        assert_eq!(
            converted["media"]["tracks"][0]["trackContentId"],
            "http://192.168.1.2:4000/t/subtitles.vtt"
        );
        assert_eq!(converted["activeTrackIds"], json!([1]));
    }
}
