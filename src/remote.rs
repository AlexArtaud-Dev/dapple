//! Driving a cast from another process. A screen such as an Apple TV keeps one session per
//! sender: `pause` from a second process would open a second session and cut the film. So the
//! casting process publishes its state in a small file, and takes orders from another one,
//! next to the pairings: other commands find it there and go through it.

use std::fs;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{json, Value};

use crate::airplay::store;
use crate::error::Result;
use crate::fail;
use crate::renderer::State;

/// A state not refreshed for this long belongs to a cast that is gone.
const FRESH: Duration = Duration::from_secs(6);
const POLL: Duration = Duration::from_millis(200);

/// Where casts publish their state and read their orders.
pub fn dir() -> PathBuf {
    store::default_dir().join("casts")
}

fn stem(ip: IpAddr) -> String {
    ip.to_string().replace(':', "_")
}

fn state_path(dir: &Path, ip: IpAddr) -> PathBuf {
    dir.join(format!("{}.json", stem(ip)))
}

fn order_path(dir: &Path, ip: IpAddr) -> PathBuf {
    dir.join(format!("{}.order", stem(ip)))
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

fn state_name(state: State) -> &'static str {
    match state {
        State::Idle => "idle",
        State::Buffering => "buffering",
        State::Playing => "playing",
        State::Paused => "paused",
    }
}

/// The state a casting process publishes, removed when the cast ends.
pub struct Published {
    dir: PathBuf,
    ip: IpAddr,
}

impl Published {
    pub fn new(dir: PathBuf, ip: IpAddr) -> Published {
        let _ = fs::create_dir_all(&dir);
        // An order left by an earlier cast is not this one's.
        let _ = fs::remove_file(order_path(&dir, ip));
        Published { dir, ip }
    }

    /// Writes the current state (whole, then renamed: readers never see half of it).
    pub fn update(&self, state: State, position: f64, duration: f64, volume: Option<u8>) {
        let value = json!({
            "state": state_name(state),
            "position": position,
            "duration": duration,
            "volume": volume,
            "updated": now(),
        });
        let path = state_path(&self.dir, self.ip);
        let partial = path.with_extension("json.part");
        if fs::write(&partial, value.to_string()).is_ok() {
            let _ = fs::rename(&partial, &path);
        }
    }

    /// The order another process left, taken once.
    pub fn take_order(&self) -> Option<String> {
        let path = order_path(&self.dir, self.ip);
        let order = fs::read_to_string(&path).ok()?;
        let _ = fs::remove_file(&path);
        Some(order.trim().to_string())
    }
}

impl Drop for Published {
    fn drop(&mut self) {
        let _ = fs::remove_file(state_path(&self.dir, self.ip));
        let _ = fs::remove_file(order_path(&self.dir, self.ip));
    }
}

/// The state of a cast running for this screen, when one is.
pub fn live(dir: &Path, ip: IpAddr) -> Option<Value> {
    let value: Value = serde_json::from_str(&fs::read_to_string(state_path(dir, ip)).ok()?).ok()?;
    let updated = value["updated"].as_u64()?;
    (now().saturating_sub(updated) <= FRESH.as_secs()).then_some(value)
}

/// Hands an order to the cast running for this screen: false when none runs, an error when it
/// did not take the order in time.
pub fn send(dir: &Path, ip: IpAddr, order: &str, wait: Duration) -> Result<bool> {
    if live(dir, ip).is_none() {
        return Ok(false);
    }
    let path = order_path(dir, ip);
    let partial = path.with_extension("order.part");
    fs::write(&partial, order)?;
    fs::rename(&partial, &path)?;
    let deadline = Instant::now() + wait;
    while Instant::now() < deadline {
        if !path.exists() {
            return Ok(true);
        }
        thread::sleep(POLL);
    }
    let _ = fs::remove_file(&path);
    Err(fail!(
        "the cast to this screen did not take the order in time"
    ))
}

/// A published state as a progress line (`42% 0:04:11 / 0:09:56 playing vol 18`).
pub fn describe(value: &Value) -> String {
    let state = match value["state"].as_str() {
        Some("playing") => State::Playing,
        Some("paused") => State::Paused,
        Some("buffering") => State::Buffering,
        _ => State::Idle,
    };
    crate::session::progress_line(
        state,
        value["position"].as_f64().unwrap_or(0.0),
        value["duration"].as_f64().unwrap_or(0.0),
        value["volume"]
            .as_u64()
            .and_then(|volume| u8::try_from(volume).ok()),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn folder(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("dapple-remote-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn a_cast_publishes_its_state_and_takes_orders() {
        let dir = folder("orders");
        let ip: IpAddr = "192.168.1.114".parse().unwrap();
        assert!(live(&dir, ip).is_none());
        assert!(!send(&dir, ip, "pause", Duration::from_millis(10)).unwrap());

        let published = Published::new(dir.clone(), ip);
        published.update(State::Playing, 251.0, 596.0, Some(18));
        let state = live(&dir, ip).unwrap();
        assert_eq!(describe(&state), "42% 0:04:11 / 0:09:56 playing vol 18");

        // An order nobody takes fails in time and is withdrawn.
        assert!(send(&dir, ip, "pause", Duration::from_millis(300)).is_err());
        assert!(published.take_order().is_none());

        let taker = {
            let dir = dir.clone();
            thread::spawn(move || {
                let published = Published { dir, ip };
                for _ in 0..50 {
                    if let Some(order) = published.take_order() {
                        std::mem::forget(published);
                        return order;
                    }
                    thread::sleep(Duration::from_millis(20));
                }
                std::mem::forget(published);
                String::new()
            })
        };
        assert!(send(&dir, ip, "vol 30", Duration::from_secs(2)).unwrap());
        assert_eq!(taker.join().unwrap(), "vol 30");

        drop(published);
        assert!(live(&dir, ip).is_none());
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_old_state_is_a_cast_gone() {
        let dir = folder("stale");
        let ip: IpAddr = "::1".parse().unwrap();
        fs::create_dir_all(&dir).unwrap();
        fs::write(
            state_path(&dir, ip),
            json!({"state": "playing", "updated": 1}).to_string(),
        )
        .unwrap();
        assert!(live(&dir, ip).is_none());
        assert_eq!(
            describe(&json!({"state": "paused", "position": 5.0})),
            "0:00:05 paused"
        );
        let _ = fs::remove_dir_all(&dir);
    }
}
