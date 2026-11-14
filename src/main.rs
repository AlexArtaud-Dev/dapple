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
mod remote;
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
                Format::Choices => print!("{}", devices_choices(&devices, is_paired)),
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
            // A cast running for this screen takes the order: a second session would cut it.
            let casts = remote::dir();
            if let Action::Status = action {
                if let Some(state) = remote::live(&casts, device.ip) {
                    println!("{}", remote::describe(&state));
                    return Ok(());
                }
            } else if remote::send(
                &casts,
                device.ip,
                &action_order(&action),
                Duration::from_secs(5),
            )? {
                return Ok(());
            }
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

/// An order for a running cast, as it reads typed keys.
fn action_order(action: &Action) -> String {
    match action {
        Action::Pause => "pause".to_string(),
        Action::Resume => "play".to_string(),
        Action::Stop => "stop".to_string(),
        Action::Seek(seconds) => timecode::format(*seconds),
        Action::Volume(percent) => format!("vol {percent}"),
        Action::Status => String::new(),
    }
}

/// Whether this machine holds a pairing for the device (only AirPlay screens ask for one).
fn is_paired(device: &Device) -> bool {
    let names = renderer::pairing_names(device);
    airplay::store::load(&[names[0].as_str(), names[1].as_str()]).is_some()
}

/// One `locator<TAB>name<TAB>key=value;key=value` line per screen: what a program needs to offer
/// a choice and to say what each screen is (`type`, `model`, `pairing=needed`).
fn devices_choices(devices: &[Device], paired: impl Fn(&Device) -> bool) -> String {
    // A tab, a line break or a separator in a value would break the line format.
    let clean = |text: &str| text.replace(['\t', '\n', '\r'], " ").replace(';', ",");
    devices
        .iter()
        .map(|device| {
            let mut attributes = vec![format!("type={}", device.kind())];
            if !device.model.trim().is_empty() {
                attributes.push(format!("model={}", clean(device.model.trim())));
            }
            if matches!(device.endpoint, discovery::Endpoint::AirPlay { .. }) && !paired(device) {
                attributes.push("pairing=needed".to_string());
            }
            format!(
                "{}\t{}\t{}\n",
                device.locator,
                clean(&device.name),
                attributes.join(";")
            )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn device(name: &str, model: &str, locator: &str) -> Device {
        let found = discovery::from_locator(locator).unwrap().unwrap();
        Device {
            name: name.to_string(),
            model: model.to_string(),
            ..found
        }
    }

    #[test]
    fn lists_choices_with_what_each_screen_is() {
        let devices = [
            device(
                "Living\troom",
                "Chromecast; Ultra",
                "chromecast://192.168.1.42:8009",
            ),
            device("Home Cinema", "AppleTV14,1", "airplay://192.168.1.114:7000"),
            device("Paired", "", "airplay://192.168.1.115:7000"),
        ];
        let text = devices_choices(&devices, |device| device.name == "Paired");
        assert_eq!(
            text,
            "chromecast://192.168.1.42:8009\tLiving room\ttype=chromecast;model=Chromecast, Ultra\n\
             airplay://192.168.1.114:7000\tHome Cinema\ttype=airplay;model=AppleTV14,1;pairing=needed\n\
             airplay://192.168.1.115:7000\tPaired\ttype=airplay\n"
        );
    }
}
