//! Finds Google Cast devices (Chromecast, Google TV, Android TV, Nest) with multicast DNS.
//!
//! The question is sent from an ordinary port, which makes devices answer straight back to it
//! (a "legacy unicast" query): no need to own port 5353, which the system often holds already.

use std::net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket};
use std::time::{Duration, Instant};

const GROUP: SocketAddrV4 = SocketAddrV4::new(Ipv4Addr::new(224, 0, 0, 251), 5353);
/// Google Cast devices.
pub const CAST: &str = "_googlecast._tcp.local";
/// AirPlay devices (Apple TV, AirPlay 2 televisions and speakers).
pub const AIRPLAY: &str = "_airplay._tcp.local";

const TYPE_A: u16 = 1;
const TYPE_PTR: u16 = 12;
const TYPE_TXT: u16 = 16;
const TYPE_SRV: u16 = 33;

/// A Cast device as it announces itself.
#[derive(Debug, PartialEq)]
pub struct CastDevice {
    pub name: String,
    pub model: String,
    pub id: String,
    pub address: SocketAddr,
    /// Whether it shows video; AirPlay speakers announce themselves on the same service.
    pub video: bool,
}

#[derive(Debug, PartialEq)]
pub enum Data {
    Address(Ipv4Addr),
    Pointer(String),
    Text(Vec<(String, String)>),
    Service { port: u16, target: String },
}

#[derive(Debug, PartialEq)]
pub struct Record {
    pub name: String,
    pub data: Data,
}

/// Asks the network for the devices of a service.
pub fn search(local: Ipv4Addr, wait: Duration, service: &str) -> Vec<CastDevice> {
    let mut devices: Vec<CastDevice> = Vec::new();
    let Ok(socket) = UdpSocket::bind((local, 0)) else {
        return devices;
    };
    let _ = socket.set_read_timeout(Some(Duration::from_millis(200)));
    let question = query(service);
    for _ in 0..2 {
        let _ = socket.send_to(&question, GROUP);
    }
    let deadline = Instant::now() + wait;
    let mut buffer = [0u8; 9000];
    while Instant::now() < deadline {
        let Ok((size, source)) = socket.recv_from(&mut buffer) else {
            continue;
        };
        for device in devices_in(&parse(&buffer[..size]), source.ip(), service) {
            if !devices.iter().any(|known| known.address == device.address) {
                devices.push(device);
            }
        }
    }
    devices
}

/// A DNS question for the pointers of a service, asking for a direct answer.
pub fn query(service: &str) -> Vec<u8> {
    let mut packet = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    for label in service.split('.') {
        packet.push(label.len() as u8);
        packet.extend_from_slice(label.as_bytes());
    }
    packet.push(0);
    packet.extend_from_slice(&TYPE_PTR.to_be_bytes());
    // Class IN, with the bit asking for a unicast answer.
    packet.extend_from_slice(&0x8001u16.to_be_bytes());
    packet
}

/// Reads the records of a DNS answer, skipping what it cannot read.
pub fn parse(packet: &[u8]) -> Vec<Record> {
    read_records(packet).unwrap_or_default()
}

fn read_records(packet: &[u8]) -> Option<Vec<Record>> {
    let count = |at: usize| Some(u16::from_be_bytes([*packet.get(at)?, *packet.get(at + 1)?]));
    let questions = count(4)?;
    let total = count(6)? as usize + count(8)? as usize + count(10)? as usize;
    let mut position = 12;
    for _ in 0..questions {
        position = read_name(packet, position)?.1 + 4;
    }
    let mut records = Vec::new();
    for _ in 0..total {
        let Some((name, after)) = read_name(packet, position) else {
            break;
        };
        let Some(header) = packet.get(after..after + 10) else {
            break;
        };
        let kind = u16::from_be_bytes([header[0], header[1]]);
        let length = u16::from_be_bytes([header[8], header[9]]) as usize;
        let start = after + 10;
        let Some(data) = packet.get(start..start + length) else {
            break;
        };
        position = start + length;
        let data = match kind {
            TYPE_A if length == 4 => {
                Data::Address(Ipv4Addr::new(data[0], data[1], data[2], data[3]))
            }
            TYPE_PTR => match read_name(packet, start) {
                Some((target, _)) => Data::Pointer(target),
                None => continue,
            },
            TYPE_TXT => Data::Text(read_text(data)),
            TYPE_SRV if length > 6 => match read_name(packet, start + 6) {
                Some((target, _)) => Data::Service {
                    port: u16::from_be_bytes([data[4], data[5]]),
                    target,
                },
                None => continue,
            },
            _ => continue,
        };
        records.push(Record { name, data });
    }
    Some(records)
}

/// Reads a possibly compressed name; returns it with the position following it.
fn read_name(packet: &[u8], mut position: usize) -> Option<(String, usize)> {
    let mut labels = Vec::new();
    let mut after = None;
    let mut jumps = 0;
    loop {
        let length = *packet.get(position)? as usize;
        if length == 0 {
            position += 1;
            break;
        }
        if length & 0xC0 == 0xC0 {
            let low = *packet.get(position + 1)? as usize;
            after.get_or_insert(position + 2);
            position = ((length & 0x3F) << 8) | low;
            jumps += 1;
            // A pointer loop would never end.
            if jumps > 32 {
                return None;
            }
            continue;
        }
        let label = packet.get(position + 1..position + 1 + length)?;
        labels.push(String::from_utf8_lossy(label).into_owned());
        position += 1 + length;
    }
    Some((labels.join("."), after.unwrap_or(position)))
}

fn read_text(mut data: &[u8]) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    while let Some((&length, rest)) = data.split_first() {
        let Some(entry) = rest.get(..length as usize) else {
            break;
        };
        let entry = String::from_utf8_lossy(entry);
        if let Some((key, value)) = entry.split_once('=') {
            entries.push((key.to_string(), value.to_string()));
        }
        data = &rest[length as usize..];
    }
    entries
}

/// The records owned by a name.
fn about<'a>(records: &'a [Record], name: &'a str) -> impl Iterator<Item = &'a Record> {
    records
        .iter()
        .filter(move |record| record.name.eq_ignore_ascii_case(name))
}

/// Assembles the records of one answer into devices; `source` is who sent the answer.
pub fn devices_in(records: &[Record], source: IpAddr, service: &str) -> Vec<CastDevice> {
    let mut devices = Vec::new();
    for record in records {
        let Data::Pointer(instance) = &record.data else {
            continue;
        };
        if !record.name.eq_ignore_ascii_case(service) {
            continue;
        }
        let Some((port, target)) = about(records, instance).find_map(|r| match &r.data {
            Data::Service { port, target } => Some((*port, target.as_str())),
            _ => None,
        }) else {
            continue;
        };
        let text = |key: &str| {
            about(records, instance)
                .filter_map(|r| match &r.data {
                    Data::Text(entries) => Some(entries),
                    _ => None,
                })
                .flatten()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        };
        let ip = about(records, target)
            .find_map(|r| match r.data {
                Data::Address(ip) => Some(IpAddr::V4(ip)),
                _ => None,
            })
            .unwrap_or(source);
        let suffix = format!(".{service}");
        let label = match instance
            .to_ascii_lowercase()
            .rfind(&suffix.to_ascii_lowercase())
        {
            Some(end) => &instance[..end],
            None => instance.as_str(),
        };
        // Cast names its fields fn/md/id, AirPlay model/deviceid; AirPlay's lowest feature bit
        // is video.
        let video = match text("features") {
            Some(features) => video_feature(&features),
            None => true,
        };
        devices.push(CastDevice {
            name: text("fn").unwrap_or_else(|| label.to_string()),
            model: text("md").or_else(|| text("model")).unwrap_or_default(),
            id: text("id")
                .or_else(|| text("deviceid"))
                .unwrap_or_else(|| label.to_string()),
            address: SocketAddr::new(ip, port),
            video,
        });
    }
    devices
}

/// Reads AirPlay's `features` ("0x4A7FDFD5,0x3C177FDE", low word first): bit 0 is video.
pub fn video_feature(features: &str) -> bool {
    let low = features.split(',').next().unwrap_or("").trim();
    let digits = low.trim_start_matches("0x").trim_start_matches("0X");
    u64::from_str_radix(digits, 16).is_ok_and(|bits| bits & 1 == 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SERVICE: &str = CAST;

    #[test]
    fn reads_airplay_announcements() {
        assert!(video_feature("0x4A7FDFD5,0x3C177FDE"));
        assert!(!video_feature("0x4A7FCA00,0xBC354BD0"));
        assert!(!video_feature("nonsense"));
        let records = vec![
            Record {
                name: AIRPLAY.into(),
                data: Data::Pointer("Home Cinema._airplay._tcp.local".into()),
            },
            Record {
                name: AIRPLAY.into(),
                data: Data::Pointer("Kitchen._airplay._tcp.local".into()),
            },
            Record {
                name: "Home Cinema._airplay._tcp.local".into(),
                data: Data::Service {
                    port: 7000,
                    target: "tv.local".into(),
                },
            },
            Record {
                name: "Home Cinema._airplay._tcp.local".into(),
                data: Data::Text(vec![
                    ("model".into(), "AppleTV14,1".into()),
                    ("deviceid".into(), "AA:BB:CC:DD:EE:FF".into()),
                    ("features".into(), "0x4A7FDFD5,0x3C177FDE".into()),
                ]),
            },
            Record {
                name: "Kitchen._airplay._tcp.local".into(),
                data: Data::Service {
                    port: 7000,
                    target: "pod.local".into(),
                },
            },
            Record {
                name: "Kitchen._airplay._tcp.local".into(),
                data: Data::Text(vec![("features".into(), "0x4A7FCA00,0xBC354BD0".into())]),
            },
            Record {
                name: "tv.local".into(),
                data: Data::Address(Ipv4Addr::new(192, 168, 1, 114)),
            },
        ];
        let devices = devices_in(&records, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), AIRPLAY);
        assert_eq!(
            devices[0],
            CastDevice {
                name: "Home Cinema".into(),
                model: "AppleTV14,1".into(),
                id: "AA:BB:CC:DD:EE:FF".into(),
                address: "192.168.1.114:7000".parse().unwrap(),
                video: true
            }
        );
        assert_eq!(
            (devices[1].name.as_str(), devices[1].video),
            ("Kitchen", false)
        );
        assert!(devices_in(&records, IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), CAST).is_empty());
    }

    fn name(labels: &str) -> Vec<u8> {
        let mut bytes = Vec::new();
        for label in labels.split('.') {
            bytes.push(label.len() as u8);
            bytes.extend_from_slice(label.as_bytes());
        }
        bytes.push(0);
        bytes
    }

    fn record(owner: &[u8], kind: u16, data: &[u8]) -> Vec<u8> {
        let mut bytes = owner.to_vec();
        bytes.extend_from_slice(&kind.to_be_bytes());
        bytes.extend_from_slice(&[0, 1, 0, 0, 0, 120]);
        bytes.extend_from_slice(&(data.len() as u16).to_be_bytes());
        bytes.extend_from_slice(data);
        bytes
    }

    /// An answer shaped like a Chromecast's: the question, a pointer, then SRV, TXT and A.
    fn answer() -> Vec<u8> {
        let mut packet = vec![0, 0, 0x84, 0, 0, 1, 0, 1, 0, 0, 0, 3];
        packet.extend_from_slice(&name(SERVICE)); // at offset 12
        packet.extend_from_slice(&[0, 12, 0, 1]);
        let service_pointer = [0xC0, 12];
        let mut instance = vec![7];
        instance.extend_from_slice(b"Salon-1");
        instance.extend_from_slice(&service_pointer);
        packet.extend_from_slice(&record(&service_pointer, TYPE_PTR, &instance));
        let instance_name = name("Salon-1._googlecast._tcp.local");
        let mut service = vec![0, 0, 0, 0, 0x1F, 0x49];
        service.extend_from_slice(&name("host-1.local"));
        packet.extend_from_slice(&record(&instance_name, TYPE_SRV, &service));
        let mut text = Vec::new();
        for entry in ["id=abc123", "fn=Salon", "md=Chromecast Ultra", "flag"] {
            text.push(entry.len() as u8);
            text.extend_from_slice(entry.as_bytes());
        }
        packet.extend_from_slice(&record(&instance_name, TYPE_TXT, &text));
        packet.extend_from_slice(&record(&name("host-1.local"), TYPE_A, &[192, 168, 1, 42]));
        packet
    }

    #[test]
    fn builds_query() {
        let packet = query(SERVICE);
        assert_eq!(&packet[..12], &[0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0]);
        assert_eq!(read_name(&packet, 12).unwrap().0, SERVICE);
        assert_eq!(&packet[packet.len() - 4..], &[0, 12, 0x80, 1]);
    }

    #[test]
    fn reads_a_cast_answer() {
        let source = IpAddr::V4(Ipv4Addr::new(192, 168, 1, 99));
        let devices = devices_in(&parse(&answer()), source, CAST);
        assert_eq!(
            devices,
            vec![CastDevice {
                name: "Salon".into(),
                model: "Chromecast Ultra".into(),
                id: "abc123".into(),
                address: "192.168.1.42:8009".parse().unwrap(),
                video: true,
            }]
        );
    }

    #[test]
    fn falls_back_on_the_sender_and_the_label() {
        let records = vec![
            Record {
                name: SERVICE.into(),
                data: Data::Pointer("Tv._googlecast._tcp.local".into()),
            },
            Record {
                name: "other".into(),
                data: Data::Pointer("x".into()),
            },
            Record {
                name: SERVICE.into(),
                data: Data::Pointer("Lost._googlecast._tcp.local".into()),
            },
            Record {
                name: "Tv._googlecast._tcp.local".into(),
                data: Data::Service {
                    port: 8009,
                    target: "tv.local".into(),
                },
            },
        ];
        let source = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 7));
        let devices = devices_in(&records, source, CAST);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].name, "Tv");
        assert_eq!(devices[0].id, "Tv");
        assert_eq!(devices[0].address, "10.0.0.7:8009".parse().unwrap());
    }

    #[test]
    fn survives_garbage() {
        assert!(parse(&[]).is_empty());
        assert!(parse(&[0; 11]).is_empty());
        let full = answer();
        for cut in 12..full.len() {
            let _ = parse(&full[..cut]);
        }
        // A name pointing at itself.
        let mut looping = vec![0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0];
        looping.extend_from_slice(&[0xC0, 12]);
        assert!(parse(&looping).is_empty());
        assert!(read_text(&[5, b'a']).is_empty());
    }
}
