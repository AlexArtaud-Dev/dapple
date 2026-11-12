//! A conversion kept on disk as an HLS stream, for players that fetch a video by pieces rather
//! than read it as it arrives (Apple's): ffmpeg writes segments in a folder, the media server
//! serves them, and what was watched long ago is deleted.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::error::Result;
use crate::fail;
use crate::media::{self, Convert, Encoder, Probe, PLAYLIST};

/// Segments are kept this long behind the position: enough to step back a little.
const KEEP_BEHIND: f64 = 120.0;
const START_LIMIT: Duration = Duration::from_secs(90);

/// Where conversions are written: the data folder a host program gives (its temporary folder
/// may be read-only there), else the system's temporary folder.
pub fn work_dir(env: &dyn Fn(&str) -> Option<String>) -> PathBuf {
    ["DAPPLE_DATA", "CANOPY_PLUGIN_DATA"]
        .iter()
        .find_map(|name| env(name).filter(|value| !value.is_empty()))
        .map(|folder| PathBuf::from(folder).join("tmp"))
        .unwrap_or_else(std::env::temp_dir)
}

/// Whether a requested name is a plain file of the conversion folder, never a path.
pub fn safe_name(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

/// The segments of a playlist that end before `seconds`, in order.
pub fn segments_before(playlist: &str, seconds: f64) -> Vec<String> {
    let mut ended = Vec::new();
    let mut elapsed = 0.0;
    let mut duration = None;
    for line in playlist.lines().map(str::trim) {
        if let Some(value) = line.strip_prefix("#EXTINF:") {
            duration = value
                .split(',')
                .next()
                .and_then(|text| text.parse::<f64>().ok());
        } else if !line.is_empty() && !line.starts_with('#') {
            elapsed += duration.take().unwrap_or(0.0);
            if elapsed >= seconds {
                break;
            }
            ended.push(line.to_string());
        }
    }
    ended
}

/// Players do not start a growing stream closer than three segments from its end: with fewer
/// they fetch the same segment over and over.
const SEGMENTS_TO_START: usize = 3;

/// Whether a player can start: enough segments are listed, or the stream is complete.
pub fn can_start(playlist: &str) -> bool {
    let segments = playlist
        .lines()
        .filter(|line| line.trim_start().starts_with("#EXTINF:"))
        .count();
    segments >= SEGMENTS_TO_START || (segments > 0 && playlist.contains("#EXT-X-ENDLIST"))
}

/// Whether the playlist already lists a segment.
#[cfg(test)]
pub fn has_segment(playlist: &str) -> bool {
    playlist
        .lines()
        .any(|line| line.trim_start().starts_with("#EXTINF:"))
}

pub struct Hls {
    folder: PathBuf,
    input: PathBuf,
    probe: Option<Probe>,
    convert: Convert,
    encoder: Encoder,
    verbose: bool,
    /// Whether this ffmpeg can produce the first seconds at once.
    burst: bool,
    child: Option<Child>,
}

impl Hls {
    /// Prepares the folder of a conversion; nothing runs yet.
    pub fn new(
        base: &Path,
        token: &str,
        input: &Path,
        probe: Option<Probe>,
        convert: Convert,
        encoder: Encoder,
        verbose: bool,
    ) -> Result<Hls> {
        let folder = base.join(format!("dapple-hls-{token}"));
        std::fs::create_dir_all(&folder)
            .map_err(|error| fail!("cannot create {}: {error}", folder.display()))?;
        Ok(Hls {
            folder,
            input: input.to_path_buf(),
            probe,
            convert,
            encoder,
            verbose,
            burst: media::supports_burst(),
            child: None,
        })
    }

    pub fn folder(&self) -> &Path {
        &self.folder
    }

    fn stop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }

    fn clear(&self) {
        if let Ok(entries) = std::fs::read_dir(&self.folder) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    /// (Re)starts the conversion from a position and waits for its first segment.
    pub fn start(&mut self, start: f64) -> Result<()> {
        self.stop();
        self.clear();
        let arguments = media::hls_args(
            &self.input,
            self.probe.as_ref(),
            &self.convert,
            start,
            self.encoder,
            self.burst,
        );
        let mut child = Command::new("ffmpeg")
            .args(arguments)
            .current_dir(&self.folder)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(if self.verbose {
                Stdio::inherit()
            } else {
                Stdio::null()
            })
            .spawn()
            .map_err(|error| fail!("cannot start ffmpeg: {error}"))?;
        let deadline = Instant::now() + START_LIMIT;
        loop {
            let ready = std::fs::read_to_string(self.folder.join(PLAYLIST))
                .is_ok_and(|playlist| can_start(&playlist));
            if ready {
                break;
            }
            if let Ok(Some(status)) = child.try_wait() {
                return Err(fail!("ffmpeg could not convert this file ({status}); run with --verbose for its reason"));
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(fail!("ffmpeg did not produce any video in time"));
            }
            thread::sleep(Duration::from_millis(200));
        }
        self.child = Some(child);
        Ok(())
    }

    /// Deletes the segments watched long ago: a film must not fill the disk twice.
    pub fn trim(&self, watched: f64) {
        let Ok(playlist) = std::fs::read_to_string(self.folder.join(PLAYLIST)) else {
            return;
        };
        for name in segments_before(&playlist, watched - KEEP_BEHIND) {
            if safe_name(&name) {
                let _ = std::fs::remove_file(self.folder.join(name));
            }
        }
    }
}

impl Drop for Hls {
    fn drop(&mut self) {
        self.stop();
        let _ = std::fs::remove_dir_all(&self.folder);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PLAYLIST_TEXT: &str = "#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:4\n#EXT-X-PLAYLIST-TYPE:EVENT\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:2.000000,\nseg00000.m4s\n#EXTINF:4.000000,\nseg00001.m4s\n#EXTINF:2.500000,\nseg00002.m4s\n";

    #[test]
    fn finds_watched_segments() {
        assert_eq!(segments_before(PLAYLIST_TEXT, 0.0), Vec::<String>::new());
        assert_eq!(segments_before(PLAYLIST_TEXT, 2.0), Vec::<String>::new());
        assert_eq!(segments_before(PLAYLIST_TEXT, 2.1), vec!["seg00000.m4s"]);
        assert_eq!(
            segments_before(PLAYLIST_TEXT, 6.5),
            vec!["seg00000.m4s", "seg00001.m4s"]
        );
        assert_eq!(segments_before(PLAYLIST_TEXT, 100.0).len(), 3);
        assert_eq!(segments_before(PLAYLIST_TEXT, -50.0), Vec::<String>::new());
        assert!(segments_before("", 10.0).is_empty());
    }

    #[test]
    fn knows_when_there_is_something_to_play() {
        assert!(has_segment(PLAYLIST_TEXT));
        assert!(!has_segment("#EXTM3U\n#EXT-X-VERSION:7\n"));
        assert!(can_start(PLAYLIST_TEXT));
        let one = "#EXTM3U\n#EXTINF:10.000000,\nseg00000.m4s\n";
        assert!(!can_start(one));
        assert!(can_start(&format!("{one}#EXT-X-ENDLIST\n")));
        assert!(!can_start("#EXTM3U\n#EXT-X-ENDLIST\n"));
    }

    #[test]
    fn serves_plain_names_only() {
        for good in ["index.m3u8", "init.mp4", "seg00012.m4s", "a_b-c.ts"] {
            assert!(safe_name(good), "{good}");
        }
        for bad in [
            "",
            "../secret",
            "a/b",
            "a\\b",
            ".hidden",
            "..",
            "a b",
            "é.ts",
        ] {
            assert!(!safe_name(bad), "{bad}");
        }
    }

    #[test]
    fn chooses_where_to_write() {
        let env = |pairs: &'static [(&'static str, &'static str)]| {
            move |name: &str| {
                pairs
                    .iter()
                    .find(|(key, _)| *key == name)
                    .map(|(_, value)| value.to_string())
            }
        };
        assert_eq!(
            work_dir(&env(&[("DAPPLE_DATA", "/d"), ("CANOPY_PLUGIN_DATA", "/c")])),
            PathBuf::from("/d").join("tmp")
        );
        assert_eq!(
            work_dir(&env(&[("CANOPY_PLUGIN_DATA", "/c")])),
            PathBuf::from("/c").join("tmp")
        );
        assert_eq!(work_dir(&env(&[("DAPPLE_DATA", "")])), std::env::temp_dir());
    }

    #[test]
    fn trims_and_cleans_up() {
        let base = std::env::temp_dir();
        let token = format!("test{}", std::process::id());
        let convert = media::convert_for(media::Target::AirPlay, None, 0);
        let hls = Hls::new(
            &base,
            &token,
            Path::new("film.mkv"),
            None,
            convert,
            Encoder::Cpu,
            false,
        )
        .unwrap();
        let folder = hls.folder().to_path_buf();
        let long = PLAYLIST_TEXT.replace("2.000000", "200.000000");
        std::fs::write(folder.join(PLAYLIST), long).unwrap();
        for name in ["seg00000.m4s", "seg00001.m4s", "seg00002.m4s"] {
            std::fs::write(folder.join(name), b"x").unwrap();
        }
        hls.trim(100.0);
        assert!(folder.join("seg00000.m4s").exists());
        hls.trim(322.0);
        assert!(!folder.join("seg00000.m4s").exists() && folder.join("seg00001.m4s").exists());
        hls.clear();
        assert!(!folder.join("seg00002.m4s").exists());
        drop(hls);
        assert!(!folder.exists());
    }
}
