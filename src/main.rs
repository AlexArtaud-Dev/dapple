//! dapple casts local video files to the TVs and screens on the network.

mod airplay;
mod chromecast;
mod cli;
mod debug;
mod discovery;
mod dlna;
mod error;
mod hls;
mod http;
mod media;
mod network;
mod pairing;
mod relay;
mod renderer;
mod server;
mod session;
mod subtitles;
mod timecode;

use std::time::Duration;

use cli::{Action, Command, Format, Parsed};
use discovery::Device;
use error::Result;

fn main() {
    let parsed = match cli::parse(std::env::args_os().skip(1)) {
        Ok(parsed) => parsed,
        Err(error) => {
            eprintln!("dapple: {error}\nTry 'dapple --help'.");
            std::process::exit(2);
        }
    };
    if let Err(error) = run(parsed) {
        eprintln!("dapple: {error}");
        std::process::exit(1);
    }
}

fn run(parsed: Parsed) -> Result<()> {
    if parsed.verbose {
        debug::enable();
    }
    let Parsed { command, relay, .. } = parsed;
    let relay = relay.as_deref();
    match command {
        Command::Help => println!("{}", cli::HELP),
        Command::Version => println!("dapple {}", env!("CARGO_PKG_VERSION")),
        Command::Devices { timeout, format } => {
            let devices = discovery::find(Duration::from_secs_f64(timeout), relay)?;
            match format {
                Format::Json => println!("{}", relay::to_json(&devices)),
                Format::Choices => print!("{}", devices_choices(&devices)),
                Format::Table if devices.is_empty() => {
                    println!("No screen found. Is this machine on the same network as the TV?")
                }
                Format::Table => print!("{}", devices_table(&devices)),
            }
        }
        Command::Relay { listen, timeout } => {
            let listen = listen
                .parse()
                .map_err(|_| fail!("invalid --listen \"{listen}\" (use address:port)"))?;
            relay::serve(listen, Duration::from_secs_f64(timeout))?;
        }
        Command::Pair { to, code, timeout } => pairing::run(
            &to,
            code.as_deref(),
            Duration::from_secs_f64(timeout),
            relay,
        )?,
        Command::Cast(options) => session::cast(*options, relay)?,
        Command::Control {
            to,
            action,
            timeout,
        } => {
            let device = discovery::resolve(&to, Duration::from_secs_f64(timeout), relay)?;
            let mut renderer = renderer::connect(&device)?;
            match action {
                Action::Pause => renderer.pause()?,
                Action::Resume => renderer.play()?,
                Action::Stop => renderer.stop()?,
                Action::Seek(seconds) => renderer.seek(seconds)?,
                Action::Volume(percent) => renderer.set_volume(percent)?,
                Action::Status => {
                    let status = renderer.status()?;
                    println!(
                        "{}",
                        session::progress_line(
                            status.state,
                            status.position,
                            status.duration,
                            status.volume
                        )
                    );
                }
            }
        }
    }
    Ok(())
}

/// One `locator<TAB>label` line per screen: what a program needs to offer a choice.
fn devices_choices(devices: &[Device]) -> String {
    devices
        .iter()
        .map(|device| {
            let detail = if device.model.is_empty() {
                device.kind().to_string()
            } else {
                format!("{}, {}", device.kind(), device.model)
            };
            // A tab or a line break in a name would break the line format.
            let name = device.name.replace(['\t', '\n', '\r'], " ");
            format!("{}\t{name} ({detail})\n", device.locator)
        })
        .collect()
}

fn devices_table(devices: &[Device]) -> String {
    let rows: Vec<[String; 5]> = devices
        .iter()
        .map(|device| {
            [
                device.name.clone(),
                device.kind().to_string(),
                device.model.clone(),
                device.ip.to_string(),
                device.locator.clone(),
            ]
        })
        .collect();
    let headers = ["NAME", "TYPE", "MODEL", "ADDRESS", "LOCATOR"];
    let width = |column: usize| {
        rows.iter()
            .map(|row| row[column].chars().count())
            .chain([headers[column].len()])
            .max()
            .unwrap_or(0)
    };
    let widths = [width(0), width(1), width(2), width(3)];
    let mut table = String::new();
    let mut line = |cells: [&str; 5]| {
        for (cell, width) in cells.iter().zip(widths) {
            table.push_str(&format!("{cell:<width$}  "));
        }
        table.push_str(cells[4]);
        table.push('\n');
    };
    line(headers);
    for row in &rows {
        line([&row[0], &row[1], &row[2], &row[3], &row[4]]);
    }
    table
}
