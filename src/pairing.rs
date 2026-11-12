//! `dapple pair`: the one-time pairing some screens ask for. The screen shows a code; the code
//! is typed at the terminal or, when there is none (a job started by another program), handed
//! over by a second `dapple pair --code` run.

use std::io::{self, BufRead, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use crate::airplay::{self, store};
use crate::discovery::{self, Endpoint};
use crate::error::Result;
use crate::fail;
use crate::renderer;

/// How long a pairing waits for its code.
const WAIT: Duration = Duration::from_secs(300);
const CODE_FILE: &str = "pairing-code";
const WAITING_FILE: &str = "pairing-waiting";

/// A code is four digits on Apple TVs; other screens may differ a little.
pub fn valid_code(code: &str) -> bool {
    (4..=8).contains(&code.len()) && code.bytes().all(|byte| byte.is_ascii_digit())
}

/// Whether a pairing started less than `WAIT` ago is still waiting for its code.
pub fn is_waiting(marker_age: Option<Duration>) -> bool {
    marker_age.is_some_and(|age| age <= WAIT)
}

fn age(path: &Path) -> Option<Duration> {
    let modified = std::fs::metadata(path).ok()?.modified().ok()?;
    SystemTime::now().duration_since(modified).ok()
}

/// Removes the hand-over files when the pairing ends, however it ends.
struct Waiting(PathBuf);

impl Drop for Waiting {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0.join(WAITING_FILE));
        let _ = std::fs::remove_file(self.0.join(CODE_FILE));
    }
}

/// Hands a code to the pairing that waits for it.
fn hand_over(folder: &Path, code: &str) -> Result<()> {
    if !is_waiting(age(&folder.join(WAITING_FILE))) {
        return Err(fail!(
            "no pairing is waiting for a code: start one with 'dapple pair --to <screen>' and read the code on the screen"
        ));
    }
    std::fs::write(folder.join(CODE_FILE), code)
        .map_err(|error| fail!("cannot write in {}: {error}", folder.display()))?;
    println!("Code handed to the pairing in progress.");
    Ok(())
}

/// Waits for the code: typed here when there is a terminal, handed over otherwise.
fn read_code(folder: &Path, locator: &str) -> Result<String> {
    if io::stdin().is_terminal() {
        print!("Code shown on the screen: ");
        let _ = io::stdout().flush();
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line)?;
        return Ok(line.trim().to_string());
    }
    std::fs::create_dir_all(folder)
        .map_err(|error| fail!("cannot create {}: {error}", folder.display()))?;
    let _ = std::fs::remove_file(folder.join(CODE_FILE));
    std::fs::write(folder.join(WAITING_FILE), locator)
        .map_err(|error| fail!("cannot write in {}: {error}", folder.display()))?;
    let _cleanup = Waiting(folder.to_path_buf());
    println!("The screen shows a code. Enter it within 5 minutes with: dapple pair --to {locator} --code <code>");
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if let Ok(code) = std::fs::read_to_string(folder.join(CODE_FILE)) {
            return Ok(code.trim().to_string());
        }
        thread::sleep(Duration::from_millis(500));
    }
    Err(fail!(
        "no code was entered in time: start the pairing again"
    ))
}

pub fn run(to: &str, code: Option<&str>, wait: Duration, relay: Option<&str>) -> Result<()> {
    if let Some(code) = code {
        if !valid_code(code) {
            return Err(fail!("\"{code}\" is not a pairing code (digits only)"));
        }
        return hand_over(&store::default_dir(), code);
    }
    let device = discovery::resolve(to, wait, relay)?;
    let Endpoint::AirPlay { address } = device.endpoint else {
        return Err(fail!(
            "{} needs no pairing: only AirPlay screens do",
            device.name
        ));
    };
    let pairing = airplay::Pairing::begin(address)?;
    let code = read_code(&store::default_dir(), &device.locator)?;
    if !valid_code(&code) {
        return Err(fail!("\"{code}\" is not a pairing code (digits only)"));
    }
    let credentials = pairing.finish(&code)?;
    let names = renderer::pairing_names(&device);
    let path = store::save(&[&names[0], &names[1]], &credentials)?;
    println!("Paired with {} (kept in {}).", device.name, path.display());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checks_codes() {
        assert!(valid_code("0298"));
        assert!(valid_code("12345678"));
        for bad in ["", "123", "12a4", "123456789", " 1234", "--help"] {
            assert!(!valid_code(bad), "{bad}");
        }
    }

    #[test]
    fn a_pairing_waits_five_minutes() {
        assert!(is_waiting(Some(Duration::from_secs(10))));
        assert!(!is_waiting(Some(Duration::from_secs(301))));
        assert!(!is_waiting(None));
    }

    #[test]
    fn hands_a_code_only_to_a_waiting_pairing() {
        let folder =
            std::env::temp_dir().join(format!("dapple-pairing-test-{}", std::process::id()));
        std::fs::create_dir_all(&folder).unwrap();
        assert!(hand_over(&folder, "1234")
            .unwrap_err()
            .0
            .contains("no pairing is waiting"));
        std::fs::write(folder.join(WAITING_FILE), "airplay://tv").unwrap();
        hand_over(&folder, "1234").unwrap();
        assert_eq!(
            std::fs::read_to_string(folder.join(CODE_FILE)).unwrap(),
            "1234"
        );
        drop(Waiting(folder.clone()));
        assert!(!folder.join(WAITING_FILE).exists() && !folder.join(CODE_FILE).exists());
        std::fs::remove_dir_all(folder).unwrap();
    }
}
