//! Decides how a file reaches a screen: sent as stored when the device can play it, otherwise
//! converted on the fly by ffmpeg, copying the streams the device already plays.
//!
//! The decisions are pure; running ffprobe and ffmpeg is at the bottom of the file.

use std::ffi::OsString;
use std::path::Path;
use std::process::{Command, Stdio};

/// What ffprobe found in a file.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Probe {
    /// Seconds, 0 when unknown.
    pub duration: f64,
    pub video: String,
    /// Codec of the chosen audio track, empty without audio.
    pub audio: String,
    pub pixel_format: String,
}

/// The family of device receiving the video.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Target {
    Chromecast,
    Dlna,
}

/// When to convert (`--transcode`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    Auto,
    Always,
    Never,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Encoder {
    Cpu,
    Nvenc,
}

/// A conversion: what is copied and what is re-encoded.
#[derive(Clone, Debug, PartialEq)]
pub struct Convert {
    pub target: Target,
    pub video_copy: bool,
    pub audio_copy: bool,
    pub audio_track: usize,
}

impl Convert {
    /// Chromecast plays fragmented MP4 as it arrives; TVs expect an MPEG transport stream.
    pub fn mime(&self) -> &'static str {
        match self.target {
            Target::Chromecast => "video/mp4",
            Target::Dlna => "video/mpeg",
        }
    }

    pub fn extension(&self) -> &'static str {
        match self.target {
            Target::Chromecast => "mp4",
            Target::Dlna => "ts",
        }
    }
}

/// The content type announced for a file sent as stored.
pub fn mime_for(extension: &str) -> &'static str {
    match extension {
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mov" => "video/quicktime",
        "ts" | "m2ts" | "mts" => "video/mp2t",
        "mpg" | "mpeg" => "video/mpeg",
        "wmv" => "video/x-ms-wmv",
        "flv" => "video/x-flv",
        "mp3" => "audio/mpeg",
        "flac" => "audio/flac",
        "m4a" | "aac" => "audio/mp4",
        "ogg" | "oga" | "opus" => "audio/ogg",
        "wav" => "audio/wav",
        _ => "application/octet-stream",
    }
}

/// Whether the device is expected to play the file as stored.
///
/// TVs differ too much to guess: they get the file as is unless asked otherwise. Chromecast
/// documents what it plays, so anything else is converted.
pub fn plays_directly(target: Target, probe: Option<&Probe>, extension: &str) -> bool {
    if target == Target::Dlna {
        return true;
    }
    let container = matches!(
        extension,
        "mp4" | "m4v" | "webm" | "mkv" | "mp3" | "flac" | "m4a" | "ogg" | "wav"
    );
    match probe {
        None => container,
        Some(probe) => {
            container
                && matches!(probe.video.as_str(), "" | "h264" | "vp8" | "vp9")
                && matches!(
                    probe.audio.as_str(),
                    "" | "aac" | "mp3" | "opus" | "vorbis" | "flac"
                )
        }
    }
}

/// The cheapest conversion the device will play: without a probe, everything is re-encoded.
pub fn convert_for(target: Target, probe: Option<&Probe>, audio_track: usize) -> Convert {
    let (video, audio) = probe.map_or(("", ""), |probe| {
        (probe.video.as_str(), probe.audio.as_str())
    });
    let (video_copy, audio_copy) = match target {
        Target::Chromecast => (video == "h264", matches!(audio, "aac" | "mp3")),
        Target::Dlna => (
            matches!(video, "h264" | "hevc" | "mpeg2video"),
            matches!(audio, "aac" | "ac3" | "eac3" | "mp3" | "mp2"),
        ),
    };
    Convert {
        target,
        video_copy,
        audio_copy,
        audio_track,
    }
}

/// Converted video is capped at 1080p: plenty for a TV, and it keeps the encoder real-time.
const LONGEST_EDGE: u32 = 1920;

/// Codecs NVIDIA GPUs decode among those re-encoded here, and the 4:2:0 layouts they accept:
/// such a video stays in GPU memory from decoding to encoding.
const GPU_CODECS: [&str; 3] = ["hevc", "mpeg2video", "vc1"];
const GPU_PIXEL_FORMATS: [&str; 5] = ["yuv420p", "yuvj420p", "yuv420p10le", "nv12", "p010le"];
const NVENC_RATE: [&str; 12] = [
    "-preset", "p4", "-rc", "vbr", "-cq", "26", "-b:v", "0", "-maxrate", "8M", "-bufsize", "16M",
];

/// The ffmpeg command line writing the conversion to its standard output, from `start` seconds.
pub fn ffmpeg_args(
    input: &Path,
    probe: Option<&Probe>,
    convert: &Convert,
    start: f64,
    encoder: Encoder,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = Vec::new();
    let mut push = |values: &[&str]| args.extend(values.iter().map(OsString::from));
    push(&["-hide_banner", "-loglevel", "error", "-nostdin"]);
    let on_gpu = !convert.video_copy
        && encoder == Encoder::Nvenc
        && probe.is_some_and(|probe| {
            GPU_CODECS.contains(&probe.video.as_str())
                && GPU_PIXEL_FORMATS.contains(&probe.pixel_format.as_str())
        });
    if on_gpu {
        push(&["-hwaccel", "cuda", "-hwaccel_output_format", "cuda"]);
    }
    if start > 0.0 {
        // Before the input: a fast jump to the keyframe preceding the position.
        push(&["-ss", &format!("{start:.3}")]);
    }
    push(&["-i"]);
    args.push(input.as_os_str().to_owned());
    let audio = format!("0:a:{}?", convert.audio_track);
    let mut push = |values: &[&str]| args.extend(values.iter().map(OsString::from));
    push(&["-map", "0:v:0?", "-map", &audio, "-sn", "-dn"]);

    let bounds = format!(
        "'min({LONGEST_EDGE},iw)':'min({LONGEST_EDGE},ih)':force_original_aspect_ratio=decrease:force_divisible_by=2"
    );
    if convert.video_copy {
        push(&["-c:v", "copy"]);
    } else if on_gpu {
        push(&["-c:v", "h264_nvenc"]);
        push(&NVENC_RATE);
        let scale = format!(
            "scale_cuda=w={}:format=yuv420p",
            bounds.replacen(":'", ":h='", 1)
        );
        push(&["-vf", &scale]);
    } else {
        match encoder {
            Encoder::Nvenc => {
                push(&["-c:v", "h264_nvenc"]);
                push(&NVENC_RATE);
            }
            Encoder::Cpu => push(&["-c:v", "libx264", "-preset", "veryfast", "-crf", "21"]),
        }
        push(&["-pix_fmt", "yuv420p", "-vf", &format!("scale={bounds}")]);
    }

    match (convert.audio_copy, convert.target) {
        (true, _) => push(&["-c:a", "copy"]),
        (false, Target::Chromecast) => push(&["-c:a", "aac", "-b:a", "192k", "-ac", "2"]),
        // Every TV decodes AC-3, and it keeps surround sound.
        (false, Target::Dlna) => push(&["-c:a", "ac3", "-b:a", "448k"]),
    }
    match convert.target {
        Target::Chromecast => push(&[
            "-movflags",
            "frag_keyframe+empty_moov+default_base_moof",
            "-f",
            "mp4",
        ]),
        Target::Dlna => push(&["-f", "mpegts"]),
    }
    push(&["pipe:1"]);
    args
}

/// Reads `ffprobe -print_format json` output: the first video stream and the chosen audio track.
pub fn parse_probe(json: &[u8], audio_track: usize) -> Option<Probe> {
    let value: serde_json::Value = serde_json::from_slice(json).ok()?;
    let mut probe = Probe {
        duration: value["format"]["duration"]
            .as_str()
            .and_then(|duration| duration.parse::<f64>().ok())
            .filter(|duration| duration.is_finite() && *duration > 0.0)
            .unwrap_or(0.0),
        ..Probe::default()
    };
    let mut audio_seen = 0;
    for stream in value["streams"].as_array()? {
        let codec = stream["codec_name"]
            .as_str()
            .unwrap_or("")
            .to_ascii_lowercase();
        match stream["codec_type"].as_str() {
            // Cover art is a "video" stream of one picture: not something to play.
            Some("video")
                if probe.video.is_empty() && stream["disposition"]["attached_pic"] != 1 =>
            {
                probe.video = codec;
                probe.pixel_format = stream["pix_fmt"].as_str().unwrap_or("").to_string();
            }
            Some("audio") => {
                if audio_seen == audio_track {
                    probe.audio = codec;
                }
                audio_seen += 1;
            }
            _ => {}
        }
    }
    (!probe.video.is_empty() || audio_seen > 0).then_some(probe)
}

/// Describes a file with ffprobe; `None` when ffprobe is missing or cannot read it.
pub fn probe(path: &Path, audio_track: usize) -> Option<Probe> {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-print_format", "json", "-show_entries"])
        .arg("format=duration:stream=codec_type,codec_name,pix_fmt:stream_disposition=attached_pic")
        .arg("-i")
        .arg(path)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| parse_probe(&output.stdout, audio_track))
        .flatten()
}

fn runs(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

pub fn ffmpeg_available() -> bool {
    runs("ffmpeg", &["-version"])
}

/// Encodes a blink of black on the GPU: it fails without a usable NVIDIA GPU, driver or build.
pub fn nvenc_available() -> bool {
    runs(
        "ffmpeg",
        &[
            "-hide_banner",
            "-loglevel",
            "error",
            "-nostdin",
            "-f",
            "lavfi",
            "-i",
            "color=black:s=256x144:d=0.1",
            "-c:v",
            "h264_nvenc",
            "-f",
            "null",
            "-",
        ],
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe_of(video: &str, audio: &str) -> Probe {
        Probe {
            duration: 60.0,
            video: video.into(),
            audio: audio.into(),
            pixel_format: "yuv420p".into(),
        }
    }

    fn line(args: &[OsString]) -> String {
        args.iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ")
    }

    #[test]
    fn reads_probe() {
        let json = br#"{"streams":[
            {"codec_type":"video","codec_name":"mjpeg","disposition":{"attached_pic":1}},
            {"codec_type":"video","codec_name":"HEVC","pix_fmt":"yuv420p10le","disposition":{"attached_pic":0}},
            {"codec_type":"audio","codec_name":"dts"},
            {"codec_type":"audio","codec_name":"aac"},
            {"codec_type":"subtitle","codec_name":"subrip"}],
            "format":{"duration":"5400.5"}}"#;
        let first = parse_probe(json, 0).unwrap();
        assert_eq!(
            first,
            Probe {
                duration: 5400.5,
                video: "hevc".into(),
                audio: "dts".into(),
                pixel_format: "yuv420p10le".into()
            }
        );
        assert_eq!(parse_probe(json, 1).unwrap().audio, "aac");
        assert_eq!(parse_probe(json, 5).unwrap().audio, "");
        let music = parse_probe(
            br#"{"streams":[{"codec_type":"audio","codec_name":"flac"}],"format":{}}"#,
            0,
        )
        .unwrap();
        assert_eq!(
            (music.video.as_str(), music.audio.as_str(), music.duration),
            ("", "flac", 0.0)
        );
        assert_eq!(parse_probe(br#"{"streams":[],"format":{}}"#, 0), None);
        assert_eq!(parse_probe(br#"{"format":{}}"#, 0), None);
        assert_eq!(parse_probe(b"nope", 0), None);
    }

    #[test]
    fn decides_direct_play() {
        use Target::*;
        assert!(plays_directly(Dlna, Some(&probe_of("hevc", "dts")), "mkv"));
        assert!(plays_directly(
            Chromecast,
            Some(&probe_of("h264", "aac")),
            "mp4"
        ));
        assert!(plays_directly(
            Chromecast,
            Some(&probe_of("vp9", "opus")),
            "webm"
        ));
        assert!(plays_directly(
            Chromecast,
            Some(&probe_of("h264", "")),
            "mkv"
        ));
        assert!(plays_directly(
            Chromecast,
            Some(&probe_of("", "flac")),
            "flac"
        ));
        assert!(!plays_directly(
            Chromecast,
            Some(&probe_of("hevc", "aac")),
            "mkv"
        ));
        assert!(!plays_directly(
            Chromecast,
            Some(&probe_of("h264", "ac3")),
            "mkv"
        ));
        assert!(!plays_directly(
            Chromecast,
            Some(&probe_of("h264", "aac")),
            "avi"
        ));
        assert!(plays_directly(Chromecast, None, "mp4"));
        assert!(!plays_directly(Chromecast, None, "avi"));
    }

    #[test]
    fn chooses_conversion() {
        use Target::*;
        let convert = |target, video, audio| {
            let chosen = convert_for(target, Some(&probe_of(video, audio)), 0);
            (chosen.video_copy, chosen.audio_copy)
        };
        assert_eq!(convert(Chromecast, "h264", "aac"), (true, true));
        assert_eq!(convert(Chromecast, "h264", "ac3"), (true, false));
        assert_eq!(convert(Chromecast, "hevc", "mp3"), (false, true));
        assert_eq!(convert(Dlna, "hevc", "eac3"), (true, true));
        assert_eq!(convert(Dlna, "vp9", "opus"), (false, false));
        let blind = convert_for(Chromecast, None, 2);
        assert_eq!(
            (blind.video_copy, blind.audio_copy, blind.audio_track),
            (false, false, 2)
        );
        assert_eq!((blind.mime(), blind.extension()), ("video/mp4", "mp4"));
        let tv = convert_for(Dlna, None, 0);
        assert_eq!((tv.mime(), tv.extension()), ("video/mpeg", "ts"));
    }

    #[test]
    fn builds_ffmpeg_command_lines() {
        let input = Path::new("movie.mkv");
        let remux = convert_for(Target::Chromecast, Some(&probe_of("h264", "dts")), 1);
        assert_eq!(
            line(&ffmpeg_args(input, Some(&probe_of("h264", "dts")), &remux, 0.0, Encoder::Cpu)),
            "-hide_banner -loglevel error -nostdin -i movie.mkv -map 0:v:0? -map 0:a:1? -sn -dn \
             -c:v copy -c:a aac -b:a 192k -ac 2 -movflags frag_keyframe+empty_moov+default_base_moof \
             -f mp4 pipe:1"
        );

        let hevc = probe_of("hevc", "aac");
        let encode = convert_for(Target::Chromecast, Some(&hevc), 0);
        let cpu = line(&ffmpeg_args(
            input,
            Some(&hevc),
            &encode,
            12.5,
            Encoder::Cpu,
        ));
        assert!(cpu.contains("-ss 12.500 -i movie.mkv"), "{cpu}");
        assert!(cpu.contains("-c:v libx264 -preset veryfast -crf 21 -pix_fmt yuv420p -vf scale='min(1920,iw)':'min(1920,ih)':force_original_aspect_ratio=decrease:force_divisible_by=2 -c:a copy"), "{cpu}");

        let gpu = line(&ffmpeg_args(
            input,
            Some(&hevc),
            &encode,
            0.0,
            Encoder::Nvenc,
        ));
        assert!(gpu.starts_with("-hide_banner -loglevel error -nostdin -hwaccel cuda -hwaccel_output_format cuda -i"), "{gpu}");
        assert!(gpu.contains("-c:v h264_nvenc -preset p4 -rc vbr -cq 26 -b:v 0 -maxrate 8M -bufsize 16M -vf scale_cuda=w='min(1920,iw)':h='min(1920,ih)':force_original_aspect_ratio=decrease:force_divisible_by=2:format=yuv420p"), "{gpu}");

        // 4:4:4 video and codecs the GPU does not decode are decoded on the CPU.
        let wide = Probe {
            pixel_format: "yuv444p".into(),
            ..hevc.clone()
        };
        let mixed = line(&ffmpeg_args(
            input,
            Some(&wide),
            &encode,
            0.0,
            Encoder::Nvenc,
        ));
        assert!(
            !mixed.contains("cuda")
                && mixed.contains("-c:v h264_nvenc")
                && mixed.contains("-pix_fmt yuv420p -vf scale="),
            "{mixed}"
        );
        let blind = line(&ffmpeg_args(
            input,
            None,
            &convert_for(Target::Dlna, None, 0),
            0.0,
            Encoder::Nvenc,
        ));
        assert!(
            !blind.contains("cuda") && blind.ends_with("-c:a ac3 -b:a 448k -f mpegts pipe:1"),
            "{blind}"
        );

        let tv = probe_of("hevc", "ac3");
        let copy = line(&ffmpeg_args(
            input,
            Some(&tv),
            &convert_for(Target::Dlna, Some(&tv), 0),
            0.0,
            Encoder::Nvenc,
        ));
        assert!(
            copy.contains("-c:v copy -c:a copy -f mpegts pipe:1") && !copy.contains("cuda"),
            "{copy}"
        );
    }

    #[test]
    fn names_content_types() {
        assert_eq!(mime_for("mkv"), "video/x-matroska");
        assert_eq!(mime_for("m4v"), "video/mp4");
        assert_eq!(mime_for("m2ts"), "video/mp2t");
        assert_eq!(mime_for("xyz"), "application/octet-stream");
        for (extension, family) in [
            ("webm", "video/"),
            ("avi", "video/"),
            ("mov", "video/"),
            ("mpg", "video/"),
            ("wmv", "video/"),
            ("flv", "video/"),
            ("mp3", "audio/"),
            ("flac", "audio/"),
            ("m4a", "audio/"),
            ("ogg", "audio/"),
            ("wav", "audio/"),
        ] {
            assert!(mime_for(extension).starts_with(family), "{extension}");
        }
    }
}
