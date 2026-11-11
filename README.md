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
dapple devices [--timeout <seconds>] [--json | --choices]
dapple cast <file> --to <device> [options]
dapple status|pause|resume|stop --to <device>
dapple seek <time> --to <device>
dapple volume <0-100> --to <device>
dapple relay [--listen <address:port>] [--timeout <seconds>]
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

## Where multicast does not pass: the relay

Finding screens relies on multicast, which a container on a bridge network never receives.
Rather than opening the whole host network to the program that casts, run a **relay** that
only lists screens:

```console
$ dapple relay --listen 172.31.250.1:8691
dapple relay listening on http://172.31.250.1:8691/devices
```

It answers `GET /devices` and nothing else: it plays nothing and reads no file. Other commands
ask it with `--relay <address>`; a dapple running in a container that sees no screen looks for a
relay on its gateway (the machine hosting it) by itself.

The image `ghcr.io/alexartaud-dev/dapple` (the binary alone, on `scratch`) runs the relay by
default:

```yaml
services:
  dapple-relay:
    image: ghcr.io/alexartaud-dev/dapple:latest
    command: [relay, --listen, '172.31.250.1:8691']
    network_mode: host # the only thing that needs it: multicast
    user: 65532:65532
    read_only: true
    cap_drop: [ALL]
    security_opt: [no-new-privileges:true]
    restart: unless-stopped
```

## Using it from other programs

Everything is scriptable: `dapple devices --json` lists screens with their locators,
`--plain` prints parseable progress (`42% 0:04:11 / 0:09:56 playing`), the exit code is 0 on
success, 1 on failure, 2 on a usage error, and `status`, `pause`, `resume`, `seek`, `volume`
and `stop` drive a screen from another process.

## As a Canopy plugin

dapple is also a plugin for [Canopy](https://github.com/AlexArtaud-Dev/Canopy), a self-hosted
file manager; it does not depend on Canopy in any way. Every release carries
`canopy-plugin.yaml`: add `AlexArtaud-Dev/dapple` under **Administration → Plugins → Sources**,
install it. Right-click a video, **Plugins › Cast to a screen › Cast a video**: the form opens
with the file filled in. The Commands page also offers *Find screens*, *Pause*, *Resume*,
*Jump*, *Volume* and *Stop*. A cast is a Canopy operation: its progress is live, and
cancelling it stops the film. Needs Canopy 2026.11.7 or later.

Save your screens once, on the plugin's card (**Saved values**): pick the field *Screen*, name
it ("Living room TV") and give its locator. Do the same for *Address the screen uses to reach
Canopy* and *Media server port*: every form then starts filled in, and screens are picked by
name.

Casting needs network paths that Canopy's default compose file closes on purpose:

- the engine must be able to **reach the screen** (its network must not be `internal`);
- the screen must be able to **reach the engine**: publish a port of the engine container
  and fill in *Address the screen uses to reach Canopy* (the Docker host's LAN address) and
  *Media server port* (the published port);
- *Find screens* relies on multicast, which Docker bridges do not carry: give screens by
  locator (`chromecast://192.168.1.42`, or a DLNA description URL), or run the engine with
  host networking.

## Limits today

- Tested against protocol simulators; real devices vary. Please open an issue with the
  output of `-v` when a screen misbehaves.
- Subtitles are external files only (no extraction from MKV yet, no burn-in).
- IPv4 only.
- `seek` from another process cannot jump inside a converted stream; use the keys of the
  casting process.

## Roadmap

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
