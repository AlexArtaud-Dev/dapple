//! One cast, from the file to the end of the film: serve it, start the screen, follow playback
//! and obey the keyboard.

use std::io::{self, BufRead, IsTerminal, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::{Duration, Instant};

use crate::cli::{CastOptions, EncoderChoice, Key};
use crate::discovery::{self, Endpoint};
use crate::error::Result;
use crate::fail;
use crate::media::{self, Encoder, Mode, Target};
use crate::network;
use crate::renderer::{self, Media, Renderer, State, Status};
use crate::server::{self, Conversion, MediaServer, Source};
use crate::subtitles::{self, Subtitles};
use crate::timecode;

/// A screen that has not started after this long will not.
const START_LIMIT: Duration = Duration::from_secs(45);
const PLAIN_EVERY: Duration = Duration::from_secs(5);

enum Input {
    Key(Key),
    Interrupt,
}

/// Whether to convert: asked for, needed to pick an audio track, or the device cannot play
/// the file as stored.
pub fn should_convert(
    mode: Mode,
    can_convert: bool,
    plays_directly: bool,
    audio_chosen: bool,
) -> Result<bool> {
    match mode {
        Mode::Never => Ok(false),
        Mode::Always if !can_convert => Err(fail!("--transcode always needs ffmpeg in the PATH")),
        Mode::Always => Ok(true),
        Mode::Auto => Ok(can_convert && (audio_chosen || !plays_directly)),
    }
}

/// A progress line: `42% 0:12:03 / 1:45:00 playing`.
pub fn progress_line(state: State, position: f64, duration: f64, volume: Option<u8>) -> String {
    let state = match state {
        State::Idle => "idle",
        State::Buffering => "buffering",
        State::Playing => "playing",
        State::Paused => "paused",
    };
    let volume = volume
        .map(|volume| format!(" vol {volume}"))
        .unwrap_or_default();
    if duration > 0.0 {
        let percent = (position / duration * 100.0).clamp(0.0, 100.0) as u8;
        format!(
            "{percent}% {} / {} {state}{volume}",
            timecode::format(position),
            timecode::format(duration)
        )
    } else {
        format!("{} {state}{volume}", timecode::format(position))
    }
}

/// Keeps a jump inside the film.
pub fn clamp_seek(target: f64, duration: f64) -> f64 {
    let end = if duration > 1.0 {
        duration - 1.0
    } else {
        f64::MAX
    };
    target.clamp(0.0, end)
}

struct Cast {
    renderer: Box<dyn Renderer>,
    base: String,
    server: MediaServer,
    title: String,
    direct_mime: String,
    direct_extension: String,
    converted_mime: Option<(&'static str, &'static str)>,
    subtitles: Subtitles,
    duration: Option<f64>,
    /// Sending a converted stream rather than the file.
    converted: bool,
    /// Where the converted stream being played starts in the film.
    offset: f64,
}

impl Cast {
    fn url(&self, name: &str) -> String {
        format!("{}{}", self.base, self.server.path(name))
    }

    fn media(&self, start: f64) -> Media {
        let (url, mime) = match (self.converted, self.converted_mime) {
            (true, Some((mime, extension))) => (
                format!(
                    "{}?start={start:.3}",
                    self.url(&format!("stream.{extension}"))
                ),
                mime.to_string(),
            ),
            _ => (
                self.url(&format!("file.{}", self.direct_extension)),
                self.direct_mime.clone(),
            ),
        };
        Media {
            url,
            mime,
            title: self.title.clone(),
            subtitles_vtt: self
                .subtitles
                .vtt
                .as_ref()
                .map(|_| self.url("subtitles.vtt")),
            subtitles_srt: self
                .subtitles
                .srt
                .as_ref()
                .map(|_| self.url("subtitles.srt")),
            start,
            duration: self.duration,
            seekable: !self.converted,
        }
    }

    fn load(&mut self, start: f64) -> Result<()> {
        let media = self.media(start);
        self.renderer.load(&media)?;
        self.offset = if self.converted { start } else { 0.0 };
        Ok(())
    }

    /// A converted stream cannot be jumped in: it is restarted from the new position.
    fn seek(&mut self, target: f64) -> Result<()> {
        if self.converted {
            self.load(target)
        } else {
            self.renderer.seek(target)
        }
    }

    fn position(&self, status: &Status) -> f64 {
        self.offset + status.position
    }

    fn duration(&self, status: &Status) -> f64 {
        self.duration.unwrap_or(status.duration)
    }
}

pub fn cast(options: CastOptions, relay: Option<&str>) -> Result<()> {
    let file = options
        .file
        .canonicalize()
        .ok()
        .filter(|path| path.is_file())
        .ok_or_else(|| fail!("{} is not a file", options.file.display()))?;
    let subtitles = match &options.subtitles {
        Some(path) => subtitles::load(path)?,
        None => Subtitles::default(),
    };
    let device = discovery::resolve(&options.to, Duration::from_secs_f64(options.timeout), relay)?;
    let target = match device.endpoint {
        Endpoint::Chromecast { .. } => Target::Chromecast,
        Endpoint::Dlna { .. } => Target::Dlna,
    };

    let extension = file
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let audio_track = options.audio.unwrap_or(0);
    let probe = media::probe(&file, audio_track);
    let can_convert = media::ffmpeg_available();
    let directly = media::plays_directly(target, probe.as_ref(), &extension);
    let converted = should_convert(
        options.transcode,
        can_convert,
        directly,
        options.audio.is_some(),
    )?;
    let conversion = can_convert.then(|| {
        let convert = media::convert_for(target, probe.as_ref(), audio_track);
        let encoder = match options.encoder {
            EncoderChoice::Fixed(encoder) => encoder,
            // Probing the GPU takes a moment: only when something will be encoded.
            EncoderChoice::Auto if !convert.video_copy && media::nvenc_available() => {
                Encoder::Nvenc
            }
            EncoderChoice::Auto => Encoder::Cpu,
        };
        Conversion {
            convert,
            probe: probe.clone(),
            encoder,
        }
    });
    let converted_mime = conversion
        .as_ref()
        .map(|conversion| (conversion.convert.mime(), conversion.convert.extension()));
    let how = match (&conversion, converted) {
        (Some(conversion), true) => match (conversion.convert.video_copy, conversion.encoder) {
            (true, _) => "repackaged on the fly",
            (false, Encoder::Nvenc) => "converted on the GPU",
            (false, Encoder::Cpu) => "converted on the CPU",
        },
        _ => "sent as stored",
    };

    let host = match &options.host {
        Some(host) => host.clone(),
        None => network::local_ip_for(device.ip)
            .ok_or_else(|| {
                fail!(
                    "cannot tell which address {} can reach this machine at (use --host)",
                    device.ip
                )
            })?
            .to_string(),
    };
    let server = server::start(
        SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), options.port),
        Source {
            path: file.clone(),
            mime: media::mime_for(&extension).to_string(),
            conversion,
            vtt: subtitles.vtt.clone(),
            srt: subtitles.srt.clone(),
            verbose: options.verbose,
        },
    )?;
    let title = options.title.clone().unwrap_or_else(|| {
        file.file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Video".to_string())
    });

    let mut cast = Cast {
        renderer: renderer::connect(&device)?,
        base: format!("http://{host}:{}", server.port()),
        server,
        title,
        direct_mime: media::mime_for(&extension).to_string(),
        direct_extension: if extension.is_empty() {
            "bin".to_string()
        } else {
            extension
        },
        converted_mime,
        subtitles,
        duration: probe
            .as_ref()
            .map(|probe| probe.duration)
            .filter(|duration| *duration > 0.0),
        converted,
        offset: 0.0,
    };
    println!(
        "Casting \"{}\" to {} ({}, {how})",
        cast.title,
        device.name,
        device.kind()
    );
    if let Err(error) = cast.load(options.start) {
        // The device refused the file as stored: convert it, if that was left to us.
        if cast.converted || cast.converted_mime.is_none() || options.transcode != Mode::Auto {
            return Err(error);
        }
        println!("The device refused the file as stored ({error}); converting it.");
        cast.converted = true;
        cast.load(options.start)?;
    }
    if let Some(volume) = options.volume {
        cast.renderer.set_volume(volume)?;
    }
    follow(&mut cast, options.plain || !io::stdout().is_terminal())
}

fn inputs() -> mpsc::Receiver<Input> {
    let (sender, receiver) = mpsc::channel();
    let interrupt = sender.clone();
    // Without a handler the screen would keep waiting for a server that is gone.
    let _ = ctrlc::set_handler(move || {
        let _ = interrupt.send(Input::Interrupt);
    });
    thread::spawn(move || {
        for line in io::stdin().lock().lines().map_while(|line| line.ok()) {
            if let Some(key) = crate::cli::parse_key(&line) {
                if sender.send(Input::Key(key)).is_err() {
                    break;
                }
            }
        }
    });
    receiver
}

fn follow(cast: &mut Cast, plain: bool) -> Result<()> {
    let receiver = inputs();
    let begun = Instant::now();
    let mut started = false;
    let mut failures = 0;
    let mut last_plain: Option<Instant> = None;
    let mut last = Status {
        state: State::Buffering,
        position: 0.0,
        duration: 0.0,
        volume: None,
    };
    if !plain {
        println!("p pause/resume · f/b ±30 s · +/- volume · <time> jump · q stop   (then Enter)");
    }
    loop {
        match receiver.recv_timeout(Duration::from_secs(1)) {
            Ok(Input::Interrupt) | Ok(Input::Key(Key::Quit)) => {
                let _ = cast.renderer.stop();
                println!("\nStopped.");
                return Ok(());
            }
            Ok(Input::Key(key)) => {
                let position = cast.position(&last);
                let duration = cast.duration(&last);
                let outcome = match key {
                    Key::Toggle if last.state == State::Paused => cast.renderer.play(),
                    Key::Toggle => cast.renderer.pause(),
                    Key::SeekBy(delta) => cast.seek(clamp_seek(position + delta, duration)),
                    Key::SeekTo(target) => cast.seek(clamp_seek(target, duration)),
                    Key::VolumeBy(delta) => match last.volume {
                        Some(volume) => cast
                            .renderer
                            .set_volume((i16::from(volume) + delta).clamp(0, 100) as u8),
                        None => Err(fail!("this device does not tell its volume")),
                    },
                    Key::Quit => Ok(()),
                };
                if let Err(error) = outcome {
                    eprintln!("\n{error}");
                }
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => thread::sleep(Duration::from_secs(1)),
        }

        last = match cast.renderer.status() {
            Ok(status) => {
                failures = 0;
                status
            }
            Err(error) => {
                failures += 1;
                if failures >= 5 {
                    return Err(fail!("lost the device: {error}"));
                }
                continue;
            }
        };
        match last.state {
            State::Playing | State::Paused => started = true,
            State::Idle if started => {
                println!("\nFinished.");
                return Ok(());
            }
            _ if !started && begun.elapsed() > START_LIMIT => {
                let _ = cast.renderer.stop();
                return Err(fail!("the device did not start playing"));
            }
            _ => {}
        }
        let line = progress_line(
            last.state,
            cast.position(&last),
            cast.duration(&last),
            last.volume,
        );
        if !plain {
            print!("\r\x1b[2K{line}");
            let _ = io::stdout().flush();
        } else if last_plain.is_none_or(|at| at.elapsed() >= PLAIN_EVERY) {
            last_plain = Some(Instant::now());
            println!("{line}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decides_to_convert() {
        assert!(!should_convert(Mode::Never, true, false, true).unwrap());
        assert!(should_convert(Mode::Always, true, true, false).unwrap());
        assert!(should_convert(Mode::Always, false, true, false).is_err());
        assert!(!should_convert(Mode::Auto, true, true, false).unwrap());
        assert!(should_convert(Mode::Auto, true, false, false).unwrap());
        assert!(should_convert(Mode::Auto, true, true, true).unwrap());
        assert!(!should_convert(Mode::Auto, false, false, true).unwrap());
    }

    #[test]
    fn writes_progress() {
        assert_eq!(
            progress_line(State::Playing, 723.0, 6300.0, Some(40)),
            "11% 0:12:03 / 1:45:00 playing vol 40"
        );
        assert_eq!(
            progress_line(State::Paused, 7000.0, 6300.0, None),
            "100% 1:56:40 / 1:45:00 paused"
        );
        assert_eq!(
            progress_line(State::Buffering, 5.0, 0.0, None),
            "0:00:05 buffering"
        );
        assert_eq!(progress_line(State::Idle, 0.0, 0.0, None), "0:00:00 idle");
    }

    #[test]
    fn keeps_jumps_inside_the_film() {
        assert_eq!(clamp_seek(-5.0, 100.0), 0.0);
        assert_eq!(clamp_seek(50.0, 100.0), 50.0);
        assert_eq!(clamp_seek(500.0, 100.0), 99.0);
        assert_eq!(clamp_seek(500.0, 0.0), 500.0);
    }
}
