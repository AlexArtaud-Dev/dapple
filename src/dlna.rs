//! Drives a UPnP/DLNA media renderer: SOAP calls to its AVTransport and RenderingControl
//! services.

use std::thread;
use std::time::Duration;

use crate::error::Result;
use crate::fail;
use crate::http;
use crate::renderer::{Media, Renderer, State, Status};
use crate::timecode;

const AV_TRANSPORT: &str = "urn:schemas-upnp-org:service:AVTransport:1";
const RENDERING_CONTROL: &str = "urn:schemas-upnp-org:service:RenderingControl:1";
const TIMEOUT: Duration = Duration::from_secs(8);

/// What DLNA players send so that a TV accepts a stream: seekable or not, played as it arrives.
const FLAGS: &str = "DLNA.ORG_FLAGS=01700000000000000000000000000000";

pub struct Dlna {
    av: String,
    rendering: Option<String>,
}

impl Dlna {
    pub fn new(av: String, rendering: Option<String>) -> Dlna {
        Dlna { av, rendering }
    }

    fn transport(&self, action: &str, arguments: &[(&str, &str)]) -> Result<String> {
        let mut all = vec![("InstanceID", "0")];
        all.extend_from_slice(arguments);
        call(&self.av, AV_TRANSPORT, action, &all)
    }

    fn rendering(&self, action: &str, arguments: &[(&str, &str)]) -> Result<String> {
        let url = self
            .rendering
            .as_deref()
            .ok_or_else(|| fail!("this device does not expose its volume"))?;
        let mut all = vec![("InstanceID", "0"), ("Channel", "Master")];
        all.extend_from_slice(arguments);
        call(url, RENDERING_CONTROL, action, &all)
    }
}

impl Renderer for Dlna {
    fn load(&mut self, media: &Media) -> Result<()> {
        // Some TVs refuse a new video while another is loaded.
        let _ = self.transport("Stop", &[]);
        let metadata = didl(media);
        self.transport(
            "SetAVTransportURI",
            &[
                ("CurrentURI", &media.url),
                ("CurrentURIMetaData", &metadata),
            ],
        )?;
        self.play()?;
        if media.start > 0.0 && media.seekable {
            // A TV accepts a seek only once it has started reading the file.
            for _ in 0..12 {
                thread::sleep(Duration::from_millis(500));
                if self.seek(media.start).is_ok() {
                    break;
                }
            }
        }
        Ok(())
    }

    fn play(&mut self) -> Result<()> {
        self.transport("Play", &[("Speed", "1")]).map(drop)
    }

    fn pause(&mut self) -> Result<()> {
        self.transport("Pause", &[]).map(drop)
    }

    fn stop(&mut self) -> Result<()> {
        self.transport("Stop", &[]).map(drop)
    }

    fn seek(&mut self, seconds: f64) -> Result<()> {
        let target = timecode::format(seconds);
        self.transport("Seek", &[("Unit", "REL_TIME"), ("Target", &target)])
            .map(drop)
    }

    fn set_volume(&mut self, percent: u8) -> Result<()> {
        let volume = percent.min(100).to_string();
        self.rendering("SetVolume", &[("DesiredVolume", &volume)])
            .map(drop)
    }

    fn status(&mut self) -> Result<Status> {
        let transport = self.transport("GetTransportInfo", &[])?;
        let state = state_of(&value(&transport, "CurrentTransportState").unwrap_or_default());
        let position = self.transport("GetPositionInfo", &[]).unwrap_or_default();
        let seconds = |tag: &str| {
            value(&position, tag)
                .and_then(|text| timecode::parse(&text))
                .unwrap_or(0.0)
        };
        let volume = self
            .rendering("GetVolume", &[])
            .ok()
            .and_then(|answer| value(&answer, "CurrentVolume"))
            .and_then(|volume| volume.parse::<u8>().ok());
        Ok(Status {
            state,
            position: seconds("RelTime"),
            duration: seconds("TrackDuration"),
            volume,
        })
    }
}

pub fn state_of(transport_state: &str) -> State {
    match transport_state {
        "PLAYING" => State::Playing,
        "PAUSED_PLAYBACK" => State::Paused,
        "TRANSITIONING" => State::Buffering,
        _ => State::Idle,
    }
}

fn call(url: &str, service: &str, action: &str, arguments: &[(&str, &str)]) -> Result<String> {
    let body = envelope(service, action, arguments);
    let soap_action = format!("\"{service}#{action}\"");
    let headers = [
        ("Content-Type", "text/xml; charset=\"utf-8\""),
        ("SOAPAction", soap_action.as_str()),
    ];
    let response = http::request("POST", url, &headers, body.as_bytes(), TIMEOUT)?;
    let text = String::from_utf8_lossy(&response.body).into_owned();
    if response.status != 200 {
        let reason = value(&text, "errorDescription")
            .or_else(|| value(&text, "errorCode").map(|code| format!("UPnP error {code}")))
            .unwrap_or_else(|| format!("HTTP {}", response.status));
        return Err(fail!("the device refused {action}: {reason}"));
    }
    Ok(text)
}

/// The SOAP request for an action.
pub fn envelope(service: &str, action: &str, arguments: &[(&str, &str)]) -> String {
    let mut body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?>\
         <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
         s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
         <s:Body><u:{action} xmlns:u=\"{service}\">"
    );
    for (name, argument) in arguments {
        body.push_str(&format!("<{name}>{}</{name}>", escape(argument)));
    }
    body.push_str(&format!("</u:{action}></s:Body></s:Envelope>"));
    body
}

pub fn escape(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for character in text.chars() {
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// The text of the first element with this name, whatever its namespace.
pub fn value(xml: &str, tag: &str) -> Option<String> {
    let document = roxmltree::Document::parse(xml).ok()?;
    let text = document
        .descendants()
        .find(|node| node.is_element() && node.tag_name().name() == tag)?
        .text()
        .unwrap_or("")
        .trim()
        .to_string();
    Some(text)
}

/// The DIDL-Lite description TVs require next to the URL: title, type and, for the many TVs
/// following Samsung's convention, where the subtitles are.
pub fn didl(media: &Media) -> String {
    let class = if media.mime.starts_with("audio/") {
        "object.item.audioItem.musicTrack"
    } else {
        "object.item.videoItem.movie"
    };
    let operations = if media.seekable { "01" } else { "00" };
    let duration = media
        .duration
        .map(|seconds| format!(" duration=\"{}\"", timecode::format(seconds)))
        .unwrap_or_default();
    let subtitles = media
        .subtitles_srt
        .as_deref()
        .map(|url| {
            let url = escape(url);
            format!(
                "<res protocolInfo=\"http-get:*:text/srt:*\">{url}</res>\
                 <sec:CaptionInfoEx sec:type=\"srt\">{url}</sec:CaptionInfoEx>\
                 <sec:CaptionInfo sec:type=\"srt\">{url}</sec:CaptionInfo>"
            )
        })
        .unwrap_or_default();
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" \
         xmlns:dc=\"http://purl.org/dc/elements/1.1/\" \
         xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\" \
         xmlns:sec=\"http://www.sec.co.kr/\">\
         <item id=\"0\" parentID=\"-1\" restricted=\"1\">\
         <dc:title>{title}</dc:title><upnp:class>{class}</upnp:class>\
         <res protocolInfo=\"http-get:*:{mime}:DLNA.ORG_OP={operations};DLNA.ORG_CI=0;{FLAGS}\"{duration}>{url}</res>\
         {subtitles}</item></DIDL-Lite>",
        title = escape(&media.title),
        mime = escape(&media.mime),
        url = escape(&media.url),
    )
}

/// The headers a TV expects on the media itself.
pub fn content_features(seekable: bool) -> String {
    let operations = if seekable { "01" } else { "00" };
    format!("DLNA.ORG_OP={operations};DLNA.ORG_CI=0;{FLAGS}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builds_envelopes() {
        let body = envelope(
            AV_TRANSPORT,
            "Seek",
            &[("InstanceID", "0"), ("Target", "a<b&c")],
        );
        assert!(body.contains("<u:Seek xmlns:u=\"urn:schemas-upnp-org:service:AVTransport:1\">"));
        assert!(body.contains("<InstanceID>0</InstanceID><Target>a&lt;b&amp;c</Target></u:Seek>"));
        assert!(roxmltree::Document::parse(&body).is_ok());
        assert_eq!(escape("\"it's\" >"), "&quot;it&apos;s&quot; &gt;");
    }

    #[test]
    fn reads_answers() {
        let answer = r#"<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/"><s:Body>
            <u:GetPositionInfoResponse xmlns:u="urn:schemas-upnp-org:service:AVTransport:1">
            <TrackDuration>1:30:00</TrackDuration><RelTime> 0:10:05 </RelTime><Empty/>
            </u:GetPositionInfoResponse></s:Body></s:Envelope>"#;
        assert_eq!(value(answer, "RelTime").as_deref(), Some("0:10:05"));
        assert_eq!(value(answer, "Empty").as_deref(), Some(""));
        assert_eq!(value(answer, "Missing"), None);
        assert_eq!(value("<broken", "RelTime"), None);
    }

    #[test]
    fn maps_states() {
        assert_eq!(state_of("PLAYING"), State::Playing);
        assert_eq!(state_of("PAUSED_PLAYBACK"), State::Paused);
        assert_eq!(state_of("TRANSITIONING"), State::Buffering);
        assert_eq!(state_of("STOPPED"), State::Idle);
        assert_eq!(state_of("NO_MEDIA_PRESENT"), State::Idle);
    }

    #[test]
    fn describes_media() {
        let mut media = Media {
            url: "http://192.168.1.2:4000/t/file.mkv?a=1&b=2".into(),
            mime: "video/x-matroska".into(),
            title: "Tom & Jerry".into(),
            seekable: true,
            duration: Some(3723.0),
            ..Media::default()
        };
        let plain = didl(&media);
        assert!(roxmltree::Document::parse(&plain).is_ok(), "{plain}");
        assert!(plain.contains("<dc:title>Tom &amp; Jerry</dc:title>"));
        assert!(plain.contains("object.item.videoItem.movie"));
        assert!(plain.contains(
            "http-get:*:video/x-matroska:DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=0170"
        ));
        assert!(plain.contains(
            " duration=\"1:02:03\">http://192.168.1.2:4000/t/file.mkv?a=1&amp;b=2</res>"
        ));
        assert!(!plain.contains("CaptionInfo"));

        media.subtitles_srt = Some("http://192.168.1.2:4000/t/subtitles.srt".into());
        media.seekable = false;
        media.duration = None;
        media.mime = "audio/flac".into();
        let rich = didl(&media);
        assert!(roxmltree::Document::parse(&rich).is_ok(), "{rich}");
        assert!(
            rich.contains("object.item.audioItem.musicTrack") && rich.contains("DLNA.ORG_OP=00;")
        );
        assert!(rich.contains("<sec:CaptionInfoEx sec:type=\"srt\">http://192.168.1.2:4000/t/subtitles.srt</sec:CaptionInfoEx>"));
        assert!(!rich.contains("duration="));
        assert!(content_features(true).starts_with("DLNA.ORG_OP=01;"));
        assert!(content_features(false).starts_with("DLNA.ORG_OP=00;"));
    }
}
