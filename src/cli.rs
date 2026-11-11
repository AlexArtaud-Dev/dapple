//! The command line: what was asked, as typed values.

use std::ffi::OsString;
use std::path::PathBuf;

use lexopt::prelude::*;

use crate::error::Result;
use crate::fail;
use crate::media::{Encoder, Mode};
use crate::timecode;

pub const HELP: &str = "\
dapple - cast local video files to the TVs and screens on your network

USAGE
  dapple devices [--timeout <seconds>] [--json | --choices]
  dapple cast <file> --to <device> [options]
  dapple status|pause|resume|stop --to <device>
  dapple seek <time> --to <device>
  dapple volume <0-100> --to <device>
  dapple relay [--listen <address:port>] [--timeout <seconds>]

<device> is a name (or part of it), an id or an address shown by 'dapple devices', or a
locator that needs no discovery: chromecast://192.168.1.50 or the http:// URL of a
DLNA description.

Where multicast does not pass (a container on a bridge network), run 'dapple relay' on a
machine that sees the screens: it lists them on http://<address>:8691/devices. Point other
commands at it with --relay <address>; in a container, a relay on the host is found alone.

CAST OPTIONS
  -t, --to <device>        Screen to play on (required)
  -s, --subtitles <file>   External subtitles (.srt or .vtt)
      --start <time>       Start position: 90, 12:30 or 1:02:03
      --audio <n>          Audio track to play, from 0 (converts the file)
      --transcode <mode>   auto (default), always or never
      --encoder <name>     auto (default), cpu or nvenc
      --title <text>       Title shown on the screen (default: the file name)
      --volume <0-100>     Set the volume before playing
      --host <address>     Address the screen must use to reach this machine
      --port <n>           Port of the media server (default: any free port)
      --timeout <seconds>  How long to look for the device (default: 3)
      --plain              One progress line every few seconds (for logs and scripts)
  -v, --verbose            Show requests from the screen and ffmpeg errors

WHILE CASTING (type, then Enter)
  p pause/resume    f/b +30 s / -30 s    +/- volume    <time> jump    q stop and quit

Converting needs ffmpeg and ffprobe in the PATH; without them files are sent as stored.";

#[derive(Debug, PartialEq)]
pub enum Action {
    Status,
    Pause,
    Resume,
    Stop,
    Seek(f64),
    Volume(u8),
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EncoderChoice {
    Auto,
    Fixed(Encoder),
}

#[derive(Debug, PartialEq)]
pub struct CastOptions {
    pub file: PathBuf,
    pub to: String,
    pub subtitles: Option<PathBuf>,
    pub start: f64,
    pub audio: Option<usize>,
    pub transcode: Mode,
    pub encoder: EncoderChoice,
    pub title: Option<String>,
    pub volume: Option<u8>,
    pub host: Option<String>,
    pub port: u16,
    pub timeout: f64,
    pub plain: bool,
    pub verbose: bool,
}

/// How `dapple devices` writes what it found.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Format {
    Table,
    Json,
    /// One `locator<TAB>label` line per screen, for programs offering a choice.
    Choices,
}

/// A command with what applies to all of them.
#[derive(Debug, PartialEq)]
pub struct Parsed {
    pub command: Command,
    /// A relay to ask for the screens (`--relay`).
    pub relay: Option<String>,
}

#[derive(Debug, PartialEq)]
pub enum Command {
    Help,
    Version,
    Devices {
        timeout: f64,
        format: Format,
    },
    Relay {
        listen: String,
        timeout: f64,
    },
    Cast(Box<CastOptions>),
    Control {
        to: String,
        action: Action,
        timeout: f64,
    },
}

const DEFAULT_TIMEOUT: f64 = 3.0;

fn seconds(text: &str, what: &str) -> Result<f64> {
    timecode::parse(text)
        .ok_or_else(|| fail!("invalid {what} \"{text}\" (use 90, 12:30 or 1:02:03)"))
}

fn percent(text: &str) -> Result<u8> {
    text.parse::<u8>()
        .ok()
        .filter(|volume| *volume <= 100)
        .ok_or_else(|| fail!("invalid volume \"{text}\" (use 0 to 100)"))
}

fn timeout(text: &str) -> Result<f64> {
    text.parse::<f64>()
        .ok()
        .filter(|value| value.is_finite() && *value > 0.0 && *value <= 60.0)
        .ok_or_else(|| fail!("invalid timeout \"{text}\" (seconds, up to 60)"))
}

/// Reads the arguments following the program name.
pub fn parse(arguments: impl IntoIterator<Item = OsString>) -> Result<Parsed> {
    let mut relay = None;
    let command = parse_command(arguments, &mut relay)?;
    Ok(Parsed { command, relay })
}

fn parse_command(
    arguments: impl IntoIterator<Item = OsString>,
    relay: &mut Option<String>,
) -> Result<Command> {
    let mut parser = lexopt::Parser::from_args(arguments);
    let mut positionals: Vec<String> = Vec::new();
    let mut to = None;
    let mut wait = DEFAULT_TIMEOUT;
    let mut format = Format::Table;
    let mut listen = format!("0.0.0.0:{}", crate::relay::PORT);
    let mut cast = CastOptions {
        file: PathBuf::new(),
        to: String::new(),
        subtitles: None,
        start: 0.0,
        audio: None,
        transcode: Mode::Auto,
        encoder: EncoderChoice::Auto,
        title: None,
        volume: None,
        host: None,
        port: 0,
        timeout: DEFAULT_TIMEOUT,
        plain: false,
        verbose: false,
    };
    // Options that only mean something when casting.
    let mut cast_only: Option<&str> = None;

    while let Some(argument) = parser.next()? {
        match argument {
            Short('h') | Long("help") => return Ok(Command::Help),
            Short('V') | Long("version") => return Ok(Command::Version),
            Short('t') | Long("to") => to = Some(parser.value()?.string()?),
            Long("timeout") => wait = timeout(&parser.value()?.string()?)?,
            Long("json") => format = Format::Json,
            Long("choices") => format = Format::Choices,
            Long("relay") => *relay = Some(parser.value()?.string()?),
            Long("listen") => listen = parser.value()?.string()?,
            Short('s') | Long("subtitles") => {
                cast.subtitles = Some(PathBuf::from(parser.value()?));
                cast_only = Some("--subtitles");
            }
            Long("start") => {
                cast.start = seconds(&parser.value()?.string()?, "start")?;
                cast_only = Some("--start");
            }
            Long("audio") => {
                let value = parser.value()?.string()?;
                cast.audio = Some(
                    value
                        .parse()
                        .map_err(|_| fail!("invalid audio track \"{value}\""))?,
                );
                cast_only = Some("--audio");
            }
            Long("transcode") => {
                let value = parser.value()?.string()?;
                cast.transcode = match value.as_str() {
                    "auto" => Mode::Auto,
                    "always" => Mode::Always,
                    "never" => Mode::Never,
                    _ => {
                        return Err(fail!(
                            "invalid --transcode \"{value}\" (auto, always or never)"
                        ))
                    }
                };
                cast_only = Some("--transcode");
            }
            Long("encoder") => {
                let value = parser.value()?.string()?;
                cast.encoder = match value.as_str() {
                    "auto" => EncoderChoice::Auto,
                    "cpu" => EncoderChoice::Fixed(Encoder::Cpu),
                    "nvenc" => EncoderChoice::Fixed(Encoder::Nvenc),
                    _ => return Err(fail!("invalid --encoder \"{value}\" (auto, cpu or nvenc)")),
                };
                cast_only = Some("--encoder");
            }
            Long("title") => {
                cast.title = Some(parser.value()?.string()?);
                cast_only = Some("--title");
            }
            Long("volume") => {
                cast.volume = Some(percent(&parser.value()?.string()?)?);
                cast_only = Some("--volume");
            }
            Long("host") => {
                cast.host = Some(parser.value()?.string()?);
                cast_only = Some("--host");
            }
            Long("port") => {
                let value = parser.value()?.string()?;
                cast.port = value
                    .parse()
                    .map_err(|_| fail!("invalid port \"{value}\""))?;
                cast_only = Some("--port");
            }
            Long("plain") => {
                cast.plain = true;
                cast_only = Some("--plain");
            }
            Short('v') | Long("verbose") => {
                cast.verbose = true;
                cast_only = Some("--verbose");
            }
            Value(value) if positionals.is_empty() => positionals.push(value.string()?),
            // A file name may not be valid Unicode: keep it as the system gave it.
            Value(value) if positionals[0] == "cast" && positionals.len() == 1 => {
                cast.file = PathBuf::from(&value);
                positionals.push(value.to_string_lossy().into_owned());
            }
            Value(value) => positionals.push(value.string()?),
            _ => return Err(argument.unexpected().into()),
        }
    }

    let Some(name) = positionals.first().map(String::as_str) else {
        return Ok(Command::Help);
    };
    let operands = positionals.len() - 1;
    let expect = |count: usize, usage: &str| {
        if operands == count {
            Ok(())
        } else {
            Err(fail!("usage: dapple {usage}"))
        }
    };
    if name != "cast" {
        if let Some(option) = cast_only {
            return Err(fail!("{option} only applies to 'dapple cast'"));
        }
    }
    let device = |usage: &str| {
        to.clone()
            .ok_or_else(|| fail!("which device? usage: dapple {usage}"))
    };
    let control = |action: Action, usage: &str| {
        Ok(Command::Control {
            to: device(usage)?,
            action,
            timeout: wait,
        })
    };
    match name {
        "help" => Ok(Command::Help),
        "version" => Ok(Command::Version),
        "devices" => {
            expect(0, "devices [--timeout <seconds>] [--json | --choices]")?;
            Ok(Command::Devices {
                timeout: wait,
                format,
            })
        }
        "relay" => {
            expect(0, "relay [--listen <address:port>] [--timeout <seconds>]")?;
            Ok(Command::Relay {
                listen,
                timeout: wait,
            })
        }
        "cast" => {
            let usage = "cast <file> --to <device> [options]";
            expect(1, usage)?;
            cast.to = device(usage)?;
            cast.timeout = wait;
            Ok(Command::Cast(Box::new(cast)))
        }
        "status" | "pause" | "resume" | "play" | "stop" => {
            let usage = format!("{name} --to <device>");
            expect(0, &usage)?;
            let action = match name {
                "status" => Action::Status,
                "pause" => Action::Pause,
                "stop" => Action::Stop,
                _ => Action::Resume,
            };
            control(action, &usage)
        }
        "seek" => {
            let usage = "seek <time> --to <device>";
            expect(1, usage)?;
            control(Action::Seek(seconds(&positionals[1], "position")?), usage)
        }
        "volume" => {
            let usage = "volume <0-100> --to <device>";
            expect(1, usage)?;
            control(Action::Volume(percent(&positionals[1])?), usage)
        }
        other => Err(fail!("unknown command \"{other}\"")),
    }
}

/// What can be typed while casting.
#[derive(Debug, PartialEq)]
pub enum Key {
    Toggle,
    Quit,
    SeekBy(f64),
    SeekTo(f64),
    VolumeBy(i16),
}

/// Reads a line typed during playback.
pub fn parse_key(line: &str) -> Option<Key> {
    match line.trim() {
        "" | "p" | "pause" | "play" => Some(Key::Toggle),
        "q" | "s" | "quit" | "stop" => Some(Key::Quit),
        "f" => Some(Key::SeekBy(30.0)),
        "b" => Some(Key::SeekBy(-30.0)),
        "+" => Some(Key::VolumeBy(5)),
        "-" => Some(Key::VolumeBy(-5)),
        other => timecode::parse(other).map(Key::SeekTo),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(line: &str) -> Result<Command> {
        parse(line.split_whitespace().map(OsString::from)).map(|parsed| parsed.command)
    }

    fn message(line: &str) -> String {
        run(line).unwrap_err().0
    }

    #[test]
    fn general_commands() {
        assert_eq!(run("").unwrap(), Command::Help);
        assert_eq!(run("help").unwrap(), Command::Help);
        assert_eq!(run("cast --help").unwrap(), Command::Help);
        assert_eq!(run("-V").unwrap(), Command::Version);
        assert_eq!(run("version").unwrap(), Command::Version);
        assert_eq!(
            run("devices").unwrap(),
            Command::Devices {
                timeout: 3.0,
                format: Format::Table
            }
        );
        assert_eq!(
            run("devices --json --timeout 5").unwrap(),
            Command::Devices {
                timeout: 5.0,
                format: Format::Json
            }
        );
        assert_eq!(
            run("devices --choices").unwrap(),
            Command::Devices {
                timeout: 3.0,
                format: Format::Choices
            }
        );
        assert_eq!(
            run("relay").unwrap(),
            Command::Relay {
                listen: "0.0.0.0:8691".into(),
                timeout: 3.0
            }
        );
        assert_eq!(
            run("relay --listen 172.31.250.1:9000 --timeout 5").unwrap(),
            Command::Relay {
                listen: "172.31.250.1:9000".into(),
                timeout: 5.0
            }
        );
        assert!(message("relay now").contains("usage: dapple relay"));
        let parsed = parse(
            "devices --relay 172.31.250.1"
                .split_whitespace()
                .map(OsString::from),
        )
        .unwrap();
        assert_eq!(parsed.relay.as_deref(), Some("172.31.250.1"));
        assert_eq!(parse([OsString::from("devices")]).unwrap().relay, None);
        assert!(message("devices extra").contains("usage: dapple devices"));
        assert!(message("devices --timeout 0").contains("invalid timeout"));
        assert!(message("devices --start 5").contains("--start only applies"));
        assert!(message("fly").contains("unknown command"));
        assert!(message("devices --nope").contains("--nope"));
    }

    #[test]
    fn cast() {
        let Command::Cast(options) = run(
            "cast film.mkv -t Salon -s film.srt --start 1:00 --audio 1 --transcode always --encoder nvenc \
             --title Film --volume 30 --host 10.0.0.2 --port 4000 --timeout 6 --plain -v",
        )
        .unwrap() else {
            panic!("not a cast");
        };
        assert_eq!(
            *options,
            CastOptions {
                file: "film.mkv".into(),
                to: "Salon".into(),
                subtitles: Some("film.srt".into()),
                start: 60.0,
                audio: Some(1),
                transcode: Mode::Always,
                encoder: EncoderChoice::Fixed(Encoder::Nvenc),
                title: Some("Film".into()),
                volume: Some(30),
                host: Some("10.0.0.2".into()),
                port: 4000,
                timeout: 6.0,
                plain: true,
                verbose: true,
            }
        );
        let Command::Cast(defaults) =
            run("cast film.mkv --to tv --transcode never --encoder cpu").unwrap()
        else {
            panic!("not a cast");
        };
        assert_eq!(
            (defaults.transcode, defaults.encoder, defaults.port),
            (Mode::Never, EncoderChoice::Fixed(Encoder::Cpu), 0)
        );
        assert!(matches!(
            run("cast a.mp4 --to tv --transcode auto --encoder auto"),
            Ok(Command::Cast(_))
        ));
        assert!(message("cast film.mkv").contains("which device?"));
        assert!(message("cast --to tv").contains("usage: dapple cast"));
        assert!(message("cast a b --to tv").contains("usage: dapple cast"));
        for (line, expected) in [
            ("cast a --to tv --start x", "invalid start"),
            ("cast a --to tv --audio x", "invalid audio track"),
            ("cast a --to tv --transcode x", "invalid --transcode"),
            ("cast a --to tv --encoder x", "invalid --encoder"),
            ("cast a --to tv --volume 101", "invalid volume"),
            ("cast a --to tv --port x", "invalid port"),
        ] {
            assert!(message(line).contains(expected), "{line}");
        }
    }

    #[test]
    fn controls() {
        let control = |line: &str| match run(line).unwrap() {
            Command::Control {
                to,
                action,
                timeout,
            } => (to, action, timeout),
            other => panic!("{other:?}"),
        };
        assert_eq!(
            control("status --to tv"),
            ("tv".into(), Action::Status, 3.0)
        );
        assert_eq!(control("pause -t tv").1, Action::Pause);
        assert_eq!(control("resume -t tv").1, Action::Resume);
        assert_eq!(control("play -t tv").1, Action::Resume);
        assert_eq!(control("stop -t tv --timeout 1").2, 1.0);
        assert_eq!(control("stop -t tv").1, Action::Stop);
        assert_eq!(control("seek 1:30 -t tv").1, Action::Seek(90.0));
        assert_eq!(control("volume 40 -t tv").1, Action::Volume(40));
        assert!(message("pause").contains("which device?"));
        assert!(message("pause now -t tv").contains("usage: dapple pause"));
        assert!(message("seek -t tv").contains("usage: dapple seek"));
        assert!(message("seek soon -t tv").contains("invalid position"));
        assert!(message("volume loud -t tv").contains("invalid volume"));
        assert!(message("volume -t tv").contains("usage: dapple volume"));
    }

    #[test]
    fn keys() {
        assert_eq!(parse_key(""), Some(Key::Toggle));
        assert_eq!(parse_key(" p "), Some(Key::Toggle));
        assert_eq!(parse_key("q"), Some(Key::Quit));
        assert_eq!(parse_key("stop"), Some(Key::Quit));
        assert_eq!(parse_key("f"), Some(Key::SeekBy(30.0)));
        assert_eq!(parse_key("b"), Some(Key::SeekBy(-30.0)));
        assert_eq!(parse_key("+"), Some(Key::VolumeBy(5)));
        assert_eq!(parse_key("-"), Some(Key::VolumeBy(-5)));
        assert_eq!(parse_key("1:00:00"), Some(Key::SeekTo(3600.0)));
        assert_eq!(parse_key("what"), None);
    }
}
