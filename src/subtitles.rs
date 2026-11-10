//! Subtitles: TVs read SubRip, Chromecast reads WebVTT. A SubRip file is offered as both.

use std::path::Path;

use crate::error::Result;
use crate::fail;

#[derive(Debug, Default, PartialEq)]
pub struct Subtitles {
    pub vtt: Option<String>,
    pub srt: Option<String>,
}

/// Loads a `.srt` or `.vtt` file.
pub fn load(path: &Path) -> Result<Subtitles> {
    let text = decode(&std::fs::read(path)?);
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match extension.as_str() {
        "srt" => Ok(Subtitles {
            vtt: Some(srt_to_vtt(&text)),
            srt: Some(text),
        }),
        "vtt" => Ok(Subtitles {
            vtt: Some(text),
            srt: None,
        }),
        _ => Err(fail!(
            "unsupported subtitles {} (use .srt or .vtt)",
            path.display()
        )),
    }
}

/// Old subtitle files are often Latin-1: when the bytes are not UTF-8, read them as such.
pub fn decode(bytes: &[u8]) -> String {
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        Err(_) => bytes.iter().map(|&byte| byte as char).collect(),
    };
    text.trim_start_matches('\u{feff}').to_string()
}

/// SubRip differs from WebVTT by its header and the comma in timestamps.
pub fn srt_to_vtt(srt: &str) -> String {
    let mut vtt = String::from("WEBVTT\n\n");
    for line in srt.lines() {
        if line.contains("-->") {
            vtt.push_str(&line.replace(',', "."));
        } else {
            vtt.push_str(line);
        }
        vtt.push('\n');
    }
    vtt
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts() {
        let srt = "1\r\n00:00:01,000 --> 00:00:02,500\r\nHello, you\r\n";
        assert_eq!(
            srt_to_vtt(srt),
            "WEBVTT\n\n1\n00:00:01.000 --> 00:00:02.500\nHello, you\n"
        );
    }

    #[test]
    fn decodes() {
        assert_eq!(decode("\u{feff}é".as_bytes()), "é");
        assert_eq!(decode(&[0xe9, b'a']), "éa");
    }
}
