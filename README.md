# dapple

Cast local video files to the TVs and screens on your network, from the command line.

One small static binary, no runtime, no account, no cloud: dapple finds the screens, serves
the file to the one you pick, and gives you a remote control in the terminal.

```console
$ dapple devices
NAME            TYPE        MODEL              ADDRESS       LOCATOR
Bedroom         chromecast  Chromecast Ultra   192.168.1.42  chromecast://192.168.1.42:8009
Living Room TV  dlna        QE55Q60            192.168.1.20  http://192.168.1.20:9197/dmr

$ dapple cast "Big Buck Bunny.mkv" --to living
Casting "Big Buck Bunny" to Living Room TV (dlna, sent as stored)
p pause/resume · f/b ±30 s · +/- volume · <time> jump · q stop   (then Enter)
42% 0:04:11 / 0:09:56 playing vol 18
```

## What it talks to

| Devices                                                                 | Protocol    | Status  |
| ----------------------------------------------------------------------- | ----------- | ------- |
| Chromecast, Google TV, Android TV (Sony, Philips, TCL…), Nest Hub       | Google Cast | yes     |
| Smart TVs (Samsung, LG, Sony, Panasonic…), Kodi, VLC, most set-top boxes | DLNA / UPnP | yes     |
| Roku, Fire TV                                                           | ECP, DIAL   | planned |
| Apple TV and AirPlay screens                                            | AirPlay     | planned |

A TV that speaks both Cast and DLNA is driven through Cast.

## Install

Download the binary for your system from the
[releases](https://github.com/AlexArtaud-Dev/dapple/releases) (Linux amd64/arm64 are static
musl builds, checksums in `SHA256SUMS`), or build it:

```console
cargo install --git https://github.com/AlexArtaud-Dev/dapple
```

Optional: `ffmpeg` and `ffprobe` in the `PATH`. Without them every file is sent as stored;
with them dapple converts on the fly what a device cannot play.

## Use

```text
dapple devices [--timeout <seconds>] [--json]
dapple cast <file> --to <device> [options]
dapple status|pause|resume|stop --to <device>
dapple seek <time> --to <device>
dapple volume <0-100> --to <device>
```

`<device>` is a name (or part of it), an id or an address shown by `dapple devices`. It can
also be a **locator**, which needs no discovery: `chromecast://192.168.1.42` or the `http://`
URL of a DLNA description. Use locators where multicast does not pass (containers, VLANs).

| Cast option               | Meaning                                                        |
| ------------------------- | -------------------------------------------------------------- |
| `-s, --subtitles <file>`  | External subtitles, `.srt` or `.vtt` (Latin-1 files are read)   |
| `--start <time>`          | Start position: `90`, `12:30` or `1:02:03`                      |
| `--audio <n>`             | Audio track to play, from 0 (converts the file)                 |
| `--transcode <mode>`      | `auto` (default), `always` or `never`                           |
| `--encoder <name>`        | `auto` (default), `cpu` or `nvenc`                              |
| `--title <text>`          | Title shown on the screen                                       |
| `--volume <0-100>`        | Set the volume before playing                                   |
| `--host <address>`        | Address the screen must use to reach this machine               |
| `--port <n>`              | Port of the media server (default: any free port)               |
| `--plain`                 | One progress line every few seconds, for logs and scripts       |
| `-v, --verbose`           | Show the screen's requests and ffmpeg's errors                  |

While casting, type a key then Enter: `p` pause/resume, `f`/`b` ±30 s, `+`/`-` volume, a time
(`1:02:03`) to jump there, `q` to stop. Ctrl+C stops the screen too.

## How a file reaches the screen

1. **As stored** when the device can play it: dapple serves the file with byte ranges, the
   device seeks by itself, and the machine does almost nothing (the copy is handed to the
   kernel).
2. **Repackaged** when only the container or the audio is the problem: the video is copied
   untouched, at no cost.
3. **Converted** otherwise: H.264 at up to 1080p, on an NVIDIA GPU when one is usable
   (decoding included for HEVC, MPEG-2 and VC-1), else on the CPU.

Chromecast documents what it plays, so dapple decides before sending; if the device still
refuses the file, it is converted automatically. TVs differ too much to guess: they get the
file as stored unless you pass `--transcode always`. Jumping in a converted stream restarts
the conversion at the new position.

## Security

- The media server exposes **one file**, under a random 128-bit path, for the time of the
  cast. Nothing else on the machine is reachable through it.
- It is plain HTTP on your local network, like every DLNA and Cast sender: do not cast on a
  network you do not trust.
- External programs (`ffmpeg`, `ffprobe`) are started with an argument list, never through a
  shell.

## Using it from other programs

Everything is scriptable: `dapple devices --json` lists screens with their locators,
`--plain` prints parseable progress (`42% 0:04:11 / 0:09:56 playing`), the exit code is 0 on
success, 1 on failure, 2 on a usage error, and `status`, `pause`, `resume`, `seek`, `volume`
and `stop` drive a screen from another process.

dapple is developed alongside [Canopy](https://github.com/AlexArtaud-Dev/Canopy), a
self-hosted file manager, which will offer it as a "Cast to…" action. It does not depend on
Canopy in any way.

## Limits today

- Tested against protocol simulators; real devices vary. Please open an issue with the
  output of `-v` when a screen misbehaves.
- Subtitles are external files only (no extraction from MKV yet, no burn-in).
- IPv4 only.
- `seek` from another process cannot jump inside a converted stream; use the keys of the
  casting process.

## Roadmap

- `dapple serve`: a small local HTTP API to list screens and drive casts, for integrations
- Roku and Fire TV, then AirPlay
- Embedded subtitles and audio track listing
- VAAPI and Quick Sync encoders

## Build and test

```console
cargo test
cargo clippy --all-targets -- -D warnings
cargo build --release
```

## License

[MIT](LICENSE)
