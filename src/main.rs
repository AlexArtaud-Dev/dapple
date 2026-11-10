//! dapple casts local video files to the TVs and screens on the network.

mod chromecast;
mod cli;
mod discovery;
mod dlna;
mod error;
mod http;
mod media;
mod network;
mod renderer;
mod server;
mod session;
mod subtitles;
mod timecode;

use std::time::Duration;

use serde_json::json;

use cli::{Action, Command};
use discovery::Device;
use error::Result;

fn main() {
    let command = match cli::parse(std::env::args_os().skip(1)) {
        Ok(command) => command,
        Err(error) => {
            eprintln!("dapple: {error}\nTry 'dapple --help'.");
            std::process::exit(2);
        }
    };
    if let Err(error) = run(command) {
        eprintln!("dapple: {error}");
        std::process::exit(1);
    }
}

fn run(command: Command) -> Result<()> {
    match command {
        Command::Help => println!("{}", cli::HELP),
        Command::Version => println!("dapple {}", env!("CARGO_PKG_VERSION")),
        Command::Devices { timeout, json } => {
            let devices = discovery::discover(Duration::from_secs_f64(timeout));
            if json {
                println!("{}", devices_json(&devices));
            } else if devices.is_empty() {
                println!("No screen found. Is this machine on the same network as the TV?");
            } else {
                print!("{}", devices_table(&devices));
            }
        }
        Command::Cast(options) => session::cast(*options)?,
        Command::Control {
            to,
            action,
            timeout,
        } => {
            let device = discovery::resolve(&to, Duration::from_secs_f64(timeout))?;
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

fn devices_json(devices: &[Device]) -> String {
    let list: Vec<_> = devices
        .iter()
        .map(|device| {
            json!({
                "name": device.name,
                "type": device.kind(),
                "model": device.model,
                "id": device.id,
                "address": device.ip.to_string(),
                "locator": device.locator,
            })
        })
        .collect();
    json!(list).to_string()
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
