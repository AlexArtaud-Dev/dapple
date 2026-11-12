//! What every kind of screen can do, whatever protocol it speaks.

use crate::airplay::{self, AirPlay};
use crate::chromecast::Chromecast;
use crate::discovery::{Device, Endpoint};
use crate::dlna::Dlna;
use crate::error::Result;
use crate::fail;

/// What a device is asked to play.
#[derive(Clone, Debug, Default)]
pub struct Media {
    pub url: String,
    pub mime: String,
    pub title: String,
    pub subtitles_vtt: Option<String>,
    pub subtitles_srt: Option<String>,
    /// Where to start, in seconds, when the device can seek in this media.
    pub start: f64,
    /// Seconds, when known.
    pub duration: Option<f64>,
    /// False for a stream converted on the fly: it has no end the device could jump to.
    pub seekable: bool,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum State {
    /// Nothing loaded, or playback ended.
    Idle,
    Buffering,
    Playing,
    Paused,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Status {
    pub state: State,
    pub position: f64,
    pub duration: f64,
    /// 0 to 100, when the device tells.
    pub volume: Option<u8>,
}

pub trait Renderer {
    fn load(&mut self, media: &Media) -> Result<()>;
    fn play(&mut self) -> Result<()>;
    fn pause(&mut self) -> Result<()>;
    fn stop(&mut self) -> Result<()>;
    fn seek(&mut self, seconds: f64) -> Result<()>;
    fn set_volume(&mut self, percent: u8) -> Result<()>;
    fn status(&mut self) -> Result<Status>;
}

/// Opens the control connection to a device.
pub fn connect(device: &Device) -> Result<Box<dyn Renderer>> {
    Ok(match &device.endpoint {
        Endpoint::Dlna { av, rendering } => Box::new(Dlna::new(av.clone(), rendering.clone())),
        Endpoint::Chromecast { address } => Box::new(Chromecast::open(*address)?),
        Endpoint::AirPlay { address } => {
            let names = pairing_names(device);
            let credentials = airplay::store::load(&[&names[0], &names[1]]).ok_or_else(|| {
                fail!(
                    "{} is not paired yet: run 'dapple pair --to {}' once, with the screen in sight",
                    device.name,
                    device.locator
                )
            })?;
            Box::new(AirPlay::open(*address, &credentials)?)
        }
    })
}

/// The names a pairing is kept under: the device's own identifier, and its address for when it
/// is reached by locator.
pub fn pairing_names(device: &Device) -> [String; 2] {
    [device.id.clone(), device.ip.to_string()]
}
