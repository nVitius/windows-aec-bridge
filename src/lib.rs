mod bridge;
pub mod cli;
mod devices;
pub mod gui;
pub mod startup;
#[cfg(target_os = "windows")]
pub mod tray;
mod wasapi_io;

use clap::{Args, ValueEnum};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Deserialize, Serialize, ValueEnum)]
pub(crate) enum ReferenceMode {
    /// Capture the actual headphones/speaker render endpoint through WASAPI loopback.
    #[default]
    Loopback,
    /// Capture an existing recording device or virtual listening mix.
    Capture,
}

#[derive(Clone, Debug, Args)]
pub(crate) struct EndpointArgs {
    /// Physical microphone capture endpoint name, substring, or stable WASAPI ID.
    #[arg(long)]
    pub(crate) mic: String,

    /// Far-end reference endpoint name, substring, or stable WASAPI ID.
    #[arg(long)]
    pub(crate) reference: String,

    /// Read the reference from playback loopback or an existing recording endpoint.
    #[arg(long, value_enum, default_value_t = ReferenceMode::Capture)]
    pub(crate) reference_mode: ReferenceMode,

    /// Virtual-cable render endpoint that receives the echo-cancelled microphone.
    #[arg(long = "handoff-render", visible_alias = "output")]
    pub(crate) output: String,
}

#[derive(Clone, Debug, Args)]
pub(crate) struct RunArgs {
    #[command(flatten)]
    pub(crate) endpoints: EndpointArgs,

    /// Hold microphone audio briefly so the render reference reaches AEC first.
    #[arg(long, default_value_t = 20, value_parser = clap::value_parser!(u32).range(0..=250))]
    pub(crate) capture_delay_ms: u32,

    /// Device/render delay estimate supplied to WebRTC AEC3.
    #[arg(long, default_value_t = 0, value_parser = clap::value_parser!(i32).range(0..=500))]
    pub(crate) stream_delay_ms: i32,

    /// Stop automatically after this many seconds; zero runs until Ctrl+C.
    #[arg(long, default_value_t = 0)]
    pub(crate) duration_seconds: u64,

    /// Route raw microphone audio through the bridge without echo cancellation.
    #[arg(long)]
    pub(crate) bypass: bool,

    /// Exercise the complete pipeline but render silence to the handoff endpoint.
    #[arg(long)]
    pub(crate) mute_output: bool,
}
