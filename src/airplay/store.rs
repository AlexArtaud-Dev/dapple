//! Where pairings are kept between runs: one small JSON file in dapple's data folder.

use std::path::PathBuf;

use serde_json::{json, Map, Value};

use super::crypto::{hex, unhex};
use super::hap::Credentials;
use crate::error::Result;
use crate::fail;

const FILE: &str = "pairings.json";

/// dapple's data folder: `DAPPLE_DATA` when set, the folder a host program gives its plugins
/// (`CANOPY_PLUGIN_DATA`), else the user's configuration folder.
pub fn data_dir(env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    let set = |name: &str| {
        env(name)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
    };
    if let Some(folder) = set("DAPPLE_DATA").or_else(|| set("CANOPY_PLUGIN_DATA")) {
        return folder;
    }
    if let Some(folder) = set("APPDATA").or_else(|| set("XDG_CONFIG_HOME")) {
        return folder.join("dapple");
    }
    match set("HOME") {
        Some(home) => home.join(".config").join("dapple"),
        None => PathBuf::from(".dapple"),
    }
}

pub fn default_dir() -> PathBuf {
    data_dir(&|name| std::env::var(name).ok())
}

fn to_json(credentials: &Credentials) -> Value {
    json!({
        "client_id": credentials.client_id,
        "seed": hex(&credentials.seed),
        "device_id": hex(&credentials.device_id),
        "device_key": hex(&credentials.device_key),
    })
}

fn from_json(value: &Value) -> Option<Credentials> {
    let bytes = |key: &str| unhex(value[key].as_str()?);
    Some(Credentials {
        client_id: value["client_id"].as_str()?.to_string(),
        seed: bytes("seed")?,
        device_id: bytes("device_id")?,
        device_key: bytes("device_key")?,
    })
}

/// Adds a pairing to the file's content, under every name the device may be asked by.
pub fn with_pairing(content: &str, keys: &[&str], credentials: &Credentials) -> String {
    let mut root: Map<String, Value> = serde_json::from_str(content).unwrap_or_default();
    let mut pairings = root
        .get("airplay")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    for key in keys {
        pairings.insert(key.to_lowercase(), to_json(credentials));
    }
    root.insert("airplay".to_string(), Value::Object(pairings));
    Value::Object(root).to_string()
}

/// Finds the pairing of a device by any of its names.
pub fn find_pairing(content: &str, keys: &[&str]) -> Option<Credentials> {
    let root: Value = serde_json::from_str(content).ok()?;
    keys.iter()
        .find_map(|key| from_json(&root["airplay"][key.to_lowercase()]))
}

pub fn save(keys: &[&str], credentials: &Credentials) -> Result<PathBuf> {
    let folder = default_dir();
    std::fs::create_dir_all(&folder)
        .map_err(|error| fail!("cannot create {}: {error}", folder.display()))?;
    let path = folder.join(FILE);
    let current = std::fs::read_to_string(&path).unwrap_or_default();
    std::fs::write(&path, with_pairing(&current, keys, credentials))
        .map_err(|error| fail!("cannot write {}: {error}", path.display()))?;
    restrict(&path);
    Ok(path)
}

pub fn load(keys: &[&str]) -> Option<Credentials> {
    find_pairing(
        &std::fs::read_to_string(default_dir().join(FILE)).ok()?,
        keys,
    )
}

/// The file holds private keys: nobody else on the machine should read it.
#[cfg(unix)]
fn restrict(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn restrict(_path: &std::path::Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn credentials(client: &str) -> Credentials {
        Credentials {
            client_id: client.into(),
            seed: vec![1; 32],
            device_id: b"AA:BB".to_vec(),
            device_key: vec![2; 32],
        }
    }

    #[test]
    fn chooses_the_data_folder() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.to_string())
            }
        };
        assert_eq!(
            data_dir(&env(&[
                ("DAPPLE_DATA", "/d"),
                ("CANOPY_PLUGIN_DATA", "/c"),
                ("HOME", "/h")
            ])),
            PathBuf::from("/d")
        );
        assert_eq!(
            data_dir(&env(&[("CANOPY_PLUGIN_DATA", "/c"), ("HOME", "/h")])),
            PathBuf::from("/c")
        );
        assert_eq!(
            data_dir(&env(&[("APPDATA", "/a"), ("HOME", "/h")])),
            PathBuf::from("/a").join("dapple")
        );
        assert_eq!(
            data_dir(&env(&[("XDG_CONFIG_HOME", "/x")])),
            PathBuf::from("/x").join("dapple")
        );
        assert_eq!(
            data_dir(&env(&[("HOME", "/h"), ("DAPPLE_DATA", "")])),
            PathBuf::from("/h").join(".config").join("dapple")
        );
        assert_eq!(data_dir(&env(&[])), PathBuf::from(".dapple"));
    }

    #[test]
    fn keeps_pairings_by_name() {
        let one = with_pairing("", &["AA:BB:CC", "192.168.1.5"], &credentials("first"));
        assert_eq!(
            find_pairing(&one, &["aa:bb:cc"]),
            Some(credentials("first"))
        );
        assert_eq!(
            find_pairing(&one, &["nope", "192.168.1.5"]),
            Some(credentials("first"))
        );
        assert_eq!(find_pairing(&one, &["nope"]), None);

        let two = with_pairing(&one, &["192.168.1.9"], &credentials("second"));
        assert_eq!(
            find_pairing(&two, &["192.168.1.9"]),
            Some(credentials("second"))
        );
        assert_eq!(
            find_pairing(&two, &["192.168.1.5"]),
            Some(credentials("first"))
        );
        let replaced = with_pairing(&two, &["192.168.1.5"], &credentials("third"));
        assert_eq!(
            find_pairing(&replaced, &["192.168.1.5"]),
            Some(credentials("third"))
        );

        assert_eq!(find_pairing("not json", &["x"]), None);
        assert_eq!(
            find_pairing(r#"{"airplay":{"x":{"client_id":"a","seed":"zz"}}}"#, &["x"]),
            None
        );
        let kept = with_pairing(r#"{"other":1}"#, &["x"], &credentials("a"));
        assert!(kept.contains("\"other\":1"));
    }
}
