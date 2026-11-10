//! Finds UPnP/DLNA media renderers (most smart TVs, Kodi, VLC) with SSDP.

use std::net::{Ipv4Addr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

use crate::http::{self, Url};

const GROUP: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(239, 255, 255, 250), 1900);

/// Some TVs answer only for the device type, others only for the service.
const TARGETS: [&str; 2] = [
    "urn:schemas-upnp-org:device:MediaRenderer:1",
    "urn:schemas-upnp-org:service:AVTransport:1",
];

/// A renderer as its description document presents it.
#[derive(Debug, PartialEq)]
pub struct Description {
    pub name: String,
    pub model: String,
    pub id: String,
    pub av_control: String,
    pub rendering_control: Option<String>,
}

/// Asks the network for renderers and returns the URLs of their descriptions.
pub fn search(local: Ipv4Addr, wait: Duration) -> Vec<String> {
    let mut locations = Vec::new();
    let Ok(socket) = UdpSocket::bind((local, 0)) else {
        return locations;
    };
    let _ = socket.set_read_timeout(Some(Duration::from_millis(200)));
    // UDP gets lost: every question is asked twice.
    for _ in 0..2 {
        for target in TARGETS {
            let question = format!(
                "M-SEARCH * HTTP/1.1\r\nHOST: {GROUP}\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: {target}\r\n\r\n"
            );
            let _ = socket.send_to(question.as_bytes(), GROUP);
        }
    }
    let deadline = Instant::now() + wait;
    let mut buffer = [0u8; 2048];
    while Instant::now() < deadline {
        // Timeouts and resets from unreachable hosts are expected: keep listening.
        let Ok((size, _)) = socket.recv_from(&mut buffer) else {
            continue;
        };
        let answer = String::from_utf8_lossy(&buffer[..size]);
        if let Some(location) = location_of(&answer) {
            if !locations.contains(&location) {
                locations.push(location);
            }
        }
    }
    locations
}

/// The LOCATION header of an SSDP answer.
pub fn location_of(answer: &str) -> Option<String> {
    answer.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        let value = value.trim();
        (name.trim().eq_ignore_ascii_case("location") && value.starts_with("http://"))
            .then(|| value.to_string())
    })
}

/// Downloads and reads a description; `None` when it is not a usable renderer.
pub fn describe(location: &str, timeout: Duration) -> Option<Description> {
    let response = http::request("GET", location, &[], &[], timeout).ok()?;
    if response.status != 200 {
        return None;
    }
    parse_description(&String::from_utf8_lossy(&response.body), location)
}

/// Finds the device carrying an AVTransport service in a description document.
pub fn parse_description(xml: &str, location: &str) -> Option<Description> {
    let document = roxmltree::Document::parse(xml).ok()?;
    let root = document.root_element();
    let base = child_text(root, "URLBase")
        .filter(|base| base.starts_with("http://"))
        .unwrap_or_else(|| location.to_string());
    let base = Url::parse(&base).ok()?;
    let main_device = root
        .children()
        .find(|node| node.has_tag_name_local("device"))?;

    for device in root
        .descendants()
        .filter(|node| node.has_tag_name_local("device"))
    {
        let Some(services) = device
            .children()
            .find(|node| node.has_tag_name_local("serviceList"))
        else {
            continue;
        };
        let control = |kind: &str| {
            services
                .children()
                .filter(|node| node.has_tag_name_local("service"))
                .find(|service| {
                    child_text(*service, "serviceType").is_some_and(|name| name.contains(kind))
                })
                .and_then(|service| child_text(service, "controlURL"))
                .map(|reference| base.join(&reference))
        };
        let Some(av_control) = control(":AVTransport:") else {
            continue;
        };
        // An embedded renderer rarely names itself: fall back on the device that contains it.
        let field = |tag: &str| {
            child_text(device, tag)
                .or_else(|| child_text(main_device, tag))
                .unwrap_or_default()
        };
        return Some(Description {
            name: field("friendlyName"),
            model: field("modelName"),
            id: field("UDN").trim_start_matches("uuid:").to_string(),
            av_control,
            rendering_control: control(":RenderingControl:"),
        });
    }
    None
}

trait LocalName {
    fn has_tag_name_local(&self, name: &str) -> bool;
}

impl LocalName for roxmltree::Node<'_, '_> {
    fn has_tag_name_local(&self, name: &str) -> bool {
        self.is_element() && self.tag_name().name() == name
    }
}

fn child_text(node: roxmltree::Node<'_, '_>, tag: &str) -> Option<String> {
    node.children()
        .find(|child| child.has_tag_name_local(tag))
        .and_then(|child| child.text())
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TV: &str = r#"<?xml version="1.0"?>
<root xmlns="urn:schemas-upnp-org:device-1-0">
  <device>
    <deviceType>urn:schemas-upnp-org:device:MediaRenderer:1</deviceType>
    <friendlyName> Living Room TV </friendlyName>
    <modelName>QE55</modelName>
    <UDN>uuid:1234-abcd</UDN>
    <serviceList>
      <service>
        <serviceType>urn:schemas-upnp-org:service:RenderingControl:1</serviceType>
        <controlURL>/upnp/control/RenderingControl1</controlURL>
      </service>
      <service>
        <serviceType>urn:schemas-upnp-org:service:AVTransport:1</serviceType>
        <controlURL>control/AVTransport1</controlURL>
      </service>
    </serviceList>
  </device>
</root>"#;

    #[test]
    fn reads_location() {
        let answer = "HTTP/1.1 200 OK\r\nST: x\r\nLocation: http://192.168.1.9:9197/dmr\r\n\r\n";
        assert_eq!(
            location_of(answer).as_deref(),
            Some("http://192.168.1.9:9197/dmr")
        );
        assert_eq!(
            location_of("HTTP/1.1 200 OK\r\nLOCATION: ftp://x\r\n"),
            None
        );
        assert_eq!(location_of("nothing"), None);
    }

    #[test]
    fn reads_description() {
        let description = parse_description(TV, "http://192.168.1.9:9197/dmr/desc.xml").unwrap();
        assert_eq!(
            description,
            Description {
                name: "Living Room TV".into(),
                model: "QE55".into(),
                id: "1234-abcd".into(),
                av_control: "http://192.168.1.9:9197/dmr/control/AVTransport1".into(),
                rendering_control: Some(
                    "http://192.168.1.9:9197/upnp/control/RenderingControl1".into()
                ),
            }
        );
    }

    #[test]
    fn reads_embedded_renderer_and_url_base() {
        let xml = r#"<root><URLBase>http://10.0.0.2:80/</URLBase><device>
            <friendlyName>Box</friendlyName><UDN>uuid:box</UDN>
            <deviceList><device><serviceList><service>
              <serviceType>urn:schemas-upnp-org:service:AVTransport:2</serviceType>
              <controlURL>/av</controlURL>
            </service></serviceList></device></deviceList>
        </device></root>"#;
        let description = parse_description(xml, "http://10.0.0.2:49152/d.xml").unwrap();
        assert_eq!(description.name, "Box");
        assert_eq!(description.id, "box");
        assert_eq!(description.av_control, "http://10.0.0.2:80/av");
        assert_eq!(description.rendering_control, None);
    }

    #[test]
    fn ignores_what_is_not_a_renderer() {
        assert_eq!(
            parse_description(
                "<root><device><friendlyName>NAS</friendlyName></device></root>",
                "http://a/"
            ),
            None
        );
        assert_eq!(
            parse_description("<root><device><serviceList/></device></root>", "http://a/"),
            None
        );
        assert_eq!(parse_description("<root/>", "http://a/"), None);
        assert_eq!(parse_description("not xml", "http://a/"), None);
        assert_eq!(parse_description(TV, "nowhere"), None);
    }
}
