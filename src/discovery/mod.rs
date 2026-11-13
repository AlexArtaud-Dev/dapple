//! The screens on the network, whatever protocol they speak.

pub mod mdns;
pub mod ssdp;

use std::net::{IpAddr, SocketAddr, ToSocketAddrs};
use std::thread;
use std::time::Duration;

use crate::error::Result;
use crate::fail;
use crate::http::Url;
use crate::network;
use crate::relay;

const CAST_SCHEME: &str = "chromecast://";
const AIRPLAY_SCHEME: &str = "airplay://";
const AIRPLAY_PORT: u16 = 7000;
const CAST_PORT: u16 = 8009;
const DESCRIPTION_TIMEOUT: Duration = Duration::from_secs(3);

/// How a device is driven.
#[derive(Clone, Debug, PartialEq)]
pub enum Endpoint {
    Dlna {
        av: String,
        rendering: Option<String>,
    },
    Chromecast {
        address: SocketAddr,
    },
    AirPlay {
        address: SocketAddr,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Device {
    pub name: String,
    pub model: String,
    pub id: String,
    /// An address that reaches the device again without discovery (`--to` accepts it).
    pub locator: String,
    pub ip: IpAddr,
    pub endpoint: Endpoint,
}

impl Device {
    pub fn kind(&self) -> &'static str {
        match self.endpoint {
            Endpoint::Dlna { .. } => "dlna",
            Endpoint::Chromecast { .. } => "chromecast",
            Endpoint::AirPlay { .. } => "airplay",
        }
    }

    /// The device as listings and relays publish it.
    pub fn to_json(&self) -> serde_json::Value {
        let mut value = serde_json::json!({
            "name": self.name,
            "type": self.kind(),
            "model": self.model,
            "id": self.id,
            "address": self.ip.to_string(),
            "locator": self.locator,
        });
        if let Endpoint::Dlna { av, rendering } = &self.endpoint {
            value["av"] = serde_json::json!(av);
            value["rendering"] = serde_json::json!(rendering);
        }
        value
    }

    /// Reads a device published by [`Device::to_json`]; `None` when something is missing.
    pub fn from_json(value: &serde_json::Value) -> Option<Device> {
        let text = |key: &str| value[key].as_str().map(str::to_string);
        let ip: IpAddr = value["address"].as_str()?.parse().ok()?;
        let locator = text("locator")?;
        let endpoint = match value["type"].as_str()? {
            "chromecast" => Endpoint::Chromecast {
                address: locator.strip_prefix(CAST_SCHEME)?.parse().ok()?,
            },
            "airplay" => Endpoint::AirPlay {
                address: locator.strip_prefix(AIRPLAY_SCHEME)?.parse().ok()?,
            },
            "dlna" => Endpoint::Dlna {
                av: text("av")?,
                rendering: text("rendering"),
            },
            _ => return None,
        };
        Some(Device {
            name: text("name")?,
            model: text("model").unwrap_or_default(),
            id: text("id").unwrap_or_default(),
            locator,
            ip,
            endpoint,
        })
    }

    fn cast(device: mdns::CastDevice) -> Device {
        Device {
            name: device.name,
            model: device.model,
            id: device.id,
            locator: format!("{CAST_SCHEME}{}", device.address),
            ip: device.address.ip(),
            endpoint: Endpoint::Chromecast {
                address: device.address,
            },
        }
    }

    fn airplay(device: mdns::CastDevice) -> Device {
        Device {
            name: device.name,
            model: device.model,
            id: device.id,
            locator: format!("{AIRPLAY_SCHEME}{}", device.address),
            ip: device.address.ip(),
            endpoint: Endpoint::AirPlay {
                address: device.address,
            },
        }
    }

    fn dlna(description: ssdp::Description, location: &str) -> Option<Device> {
        let host = Url::parse(&description.av_control).ok()?.host;
        Some(Device {
            name: description.name,
            model: description.model,
            id: description.id,
            locator: location.to_string(),
            ip: host.parse().ok()?,
            endpoint: Endpoint::Dlna {
                av: description.av_control,
                rendering: description.rendering_control,
            },
        })
    }
}

/// Looks for every screen for `wait`, both protocols at once.
pub fn discover(wait: Duration) -> Vec<Device> {
    let local = network::primary_ipv4();
    let cast = thread::spawn(move || mdns::search(local, wait, mdns::CAST));
    let airplay = thread::spawn(move || mdns::search(local, wait, mdns::AIRPLAY));
    let renderers = thread::spawn(move || {
        let lookups: Vec<_> = ssdp::search(local, wait)
            .into_iter()
            .map(|location| {
                thread::spawn(move || {
                    ssdp::describe(&location, DESCRIPTION_TIMEOUT)
                        .and_then(|description| Device::dlna(description, &location))
                })
            })
            .collect();
        lookups
            .into_iter()
            .filter_map(|lookup| lookup.join().ok().flatten())
            .collect::<Vec<_>>()
    });
    let mut devices: Vec<Device> = cast
        .join()
        .unwrap_or_default()
        .into_iter()
        .map(Device::cast)
        .collect();
    // Speakers announce themselves on the AirPlay service too: only screens are kept.
    devices.extend(
        airplay
            .join()
            .unwrap_or_default()
            .into_iter()
            .filter(|device| device.video)
            .map(Device::airplay),
    );
    for device in renderers.join().unwrap_or_default() {
        // A renderer answers once per network interface it has.
        if !devices
            .iter()
            .any(|known| known.endpoint == device.endpoint)
        {
            devices.push(device);
        }
    }
    devices.sort_by_key(|device| device.name.to_lowercase());
    devices
}

/// Reaches a device from its locator, without discovery: for networks where multicast does not
/// pass (containers, VLANs). `None` when the text is not a locator.
pub fn from_locator(text: &str) -> Option<Result<Device>> {
    if let Some(address) = text.strip_prefix(CAST_SCHEME) {
        let address = address.trim_end_matches('/');
        let with_port = if address.contains(':') {
            address.to_string()
        } else {
            format!("{address}:{CAST_PORT}")
        };
        let resolved = with_port
            .to_socket_addrs()
            .ok()
            .and_then(|mut addresses| addresses.next())
            .ok_or_else(|| fail!("cannot resolve {text}"));
        return Some(resolved.map(|address| {
            Device::cast(mdns::CastDevice {
                name: address.ip().to_string(),
                model: String::new(),
                id: address.to_string(),
                address,
                video: true,
            })
        }));
    }
    if let Some(address) = text.strip_prefix(AIRPLAY_SCHEME) {
        let address = address.trim_end_matches('/');
        let with_port = if address.contains(':') {
            address.to_string()
        } else {
            format!("{address}:{AIRPLAY_PORT}")
        };
        let resolved = with_port
            .to_socket_addrs()
            .ok()
            .and_then(|mut addresses| addresses.next())
            .ok_or_else(|| fail!("cannot resolve {text}"));
        return Some(resolved.map(|address| {
            Device::airplay(mdns::CastDevice {
                name: address.ip().to_string(),
                model: String::new(),
                id: address.ip().to_string(),
                address,
                video: true,
            })
        }));
    }
    if text.starts_with("http://") {
        let device = ssdp::describe(text, DESCRIPTION_TIMEOUT)
            .and_then(|description| Device::dlna(description, text))
            .ok_or_else(|| fail!("no media renderer is described at {text}"));
        return Some(device);
    }
    None
}

/// Chooses the device a person meant by `query`: a name (or part of it), an id or an address.
pub fn pick<'a>(devices: &'a [Device], query: &str) -> Result<&'a Device> {
    let wanted = query.trim().to_lowercase();
    let exact: Vec<&Device> = devices
        .iter()
        .filter(|device| {
            device.name.to_lowercase() == wanted
                || device.id.to_lowercase() == wanted
                || device.ip.to_string() == wanted
        })
        .collect();
    let matches = if exact.is_empty() {
        devices
            .iter()
            .filter(|device| device.name.to_lowercase().contains(&wanted))
            .collect()
    } else {
        exact
    };
    match matches.as_slice() {
        [] => Err(fail!(
            "no device matches \"{query}\" (run 'dapple devices' to list them)"
        )),
        [device] => Ok(device),
        // One TV speaking both protocols: Cast gives the finer control.
        several if several.iter().all(|device| device.ip == several[0].ip) => Ok(several
            .iter()
            .find(|device| matches!(device.endpoint, Endpoint::Chromecast { .. }))
            .unwrap_or(&several[0])),
        several => Err(fail!(
            "\"{query}\" matches several devices: {}",
            several
                .iter()
                .map(|device| format!("{} ({})", device.name, device.ip))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// The screens this program can learn of: those it sees itself, else those a relay sees.
///
/// A relay given with `--relay` is asked directly. Without one, a program running in a container
/// that sees nothing tries a relay on the machine hosting it (its gateway, then Docker Desktop's
/// host): containers on a bridge network receive no multicast, so this is how they find screens.
pub fn find(wait: Duration, relay_address: Option<&str>) -> Result<Vec<Device>> {
    if let Some(address) = relay_address {
        return relay::ask(address, wait);
    }
    let devices = discover(wait);
    if !devices.is_empty() {
        return Ok(devices);
    }
    for host in relay::locate(None) {
        if let Ok(found) = relay::ask(&host, wait) {
            return Ok(found);
        }
    }
    Ok(devices)
}

/// Finds the device named by `--to`.
pub fn resolve(query: &str, wait: Duration, relay_address: Option<&str>) -> Result<Device> {
    if let Some(device) = from_locator(query) {
        return device;
    }
    let devices = find(wait, relay_address)?;
    pick(&devices, query).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(name: &str, ip: &str, cast: bool) -> Device {
        let ip: IpAddr = ip.parse().unwrap();
        Device {
            name: name.into(),
            model: String::new(),
            id: format!("id-{name}-{cast}"),
            locator: String::new(),
            ip,
            endpoint: if cast {
                Endpoint::Chromecast {
                    address: SocketAddr::new(ip, 8009),
                }
            } else {
                Endpoint::Dlna {
                    av: format!("http://{ip}/av"),
                    rendering: None,
                }
            },
        }
    }

    #[test]
    fn picks() {
        let devices = vec![
            device("Living Room TV", "192.168.1.10", false),
            device("Living Room TV", "192.168.1.10", true),
            device("Bedroom TV", "192.168.1.11", false),
            device("Bedroom", "192.168.1.12", true),
        ];
        assert_eq!(pick(&devices, "living").unwrap().kind(), "chromecast");
        assert_eq!(
            pick(&devices, " BEDROOM ").unwrap().ip.to_string(),
            "192.168.1.12"
        );
        assert_eq!(pick(&devices, "192.168.1.11").unwrap().name, "Bedroom TV");
        assert_eq!(
            pick(&devices, "id-bedroom tv-false").unwrap().name,
            "Bedroom TV"
        );
        assert!(pick(&devices, "tv").unwrap_err().0.contains("several"));
        assert!(pick(&devices, "kitchen")
            .unwrap_err()
            .0
            .contains("no device"));
        let twins = vec![
            device("A", "10.0.0.1", false),
            device("A", "10.0.0.1", false),
        ];
        assert_eq!(pick(&twins, "a").unwrap().kind(), "dlna");
    }

    #[test]
    fn locators() {
        assert!(from_locator("Living Room").is_none());
        let device = from_locator("chromecast://192.168.1.50").unwrap().unwrap();
        assert_eq!(
            device.endpoint,
            Endpoint::Chromecast {
                address: "192.168.1.50:8009".parse().unwrap()
            }
        );
        assert_eq!(device.locator, "chromecast://192.168.1.50:8009");
        let device = from_locator("chromecast://192.168.1.50:8010/")
            .unwrap()
            .unwrap();
        assert_eq!(device.ip.to_string(), "192.168.1.50");
        assert_eq!(device.kind(), "chromecast");
    }

    #[test]
    fn devices_have_a_json_form() {
        let tv = Device {
            name: "TV".into(),
            model: "M".into(),
            id: "1".into(),
            locator: "http://192.168.1.9/d.xml".into(),
            ip: "192.168.1.9".parse().unwrap(),
            endpoint: Endpoint::Dlna {
                av: "http://192.168.1.9/av".into(),
                rendering: Some("http://192.168.1.9/rc".into()),
            },
        };
        assert_eq!(Device::from_json(&tv.to_json()), Some(tv.clone()));
        let silent = Device {
            endpoint: Endpoint::Dlna {
                av: "http://192.168.1.9/av".into(),
                rendering: None,
            },
            ..tv
        };
        assert_eq!(Device::from_json(&silent.to_json()), Some(silent.clone()));
        let cast = device("Cast", "192.168.1.42", true);
        let cast = Device {
            locator: "chromecast://192.168.1.42:8009".into(),
            ..cast
        };
        assert_eq!(Device::from_json(&cast.to_json()), Some(cast.clone()));

        let mut broken = cast.to_json();
        broken["type"] = "airplay".into();
        assert_eq!(Device::from_json(&broken), None);
        let mut broken = cast.to_json();
        broken["locator"] = "nowhere".into();
        assert_eq!(Device::from_json(&broken), None);
        let mut broken = silent.to_json();
        broken["av"] = serde_json::Value::Null;
        assert_eq!(Device::from_json(&broken), None);
        let mut broken = silent.to_json();
        broken["address"] = "tv.local".into();
        assert_eq!(Device::from_json(&broken), None);
    }

    #[test]
    fn airplay_screens_have_locators() {
        let device = from_locator("airplay://192.168.1.114").unwrap().unwrap();
        assert_eq!(device.kind(), "airplay");
        assert_eq!(device.locator, "airplay://192.168.1.114:7000");
        assert_eq!(
            device.endpoint,
            Endpoint::AirPlay {
                address: "192.168.1.114:7000".parse().unwrap()
            }
        );
        assert_eq!(Device::from_json(&device.to_json()), Some(device));
        let other = from_locator("airplay://192.168.1.114:7100/")
            .unwrap()
            .unwrap();
        assert_eq!(other.locator, "airplay://192.168.1.114:7100");
    }

    #[test]
    fn dlna_devices_need_an_address() {
        let description = |control: &str| ssdp::Description {
            name: "TV".into(),
            model: "M".into(),
            id: "1".into(),
            av_control: control.into(),
            rendering_control: None,
        };
        let device = Device::dlna(
            description("http://192.168.1.9:80/av"),
            "http://192.168.1.9/d.xml",
        )
        .unwrap();
        assert_eq!(device.kind(), "dlna");
        assert_eq!(device.locator, "http://192.168.1.9/d.xml");
        assert!(Device::dlna(description("http://tv.local/av"), "x").is_none());
    }
}
