//! Device I/O, kept behind a small surface so the rest of the program does not
//! deal with cpal directly.
//!
//! One host abstraction covers both ends of this project: CoreAudio on the Mac
//! that captures, WASAPI on the Windows box that renders. That is the reason the
//! whole thing is a single binary with two subcommands rather than two programs,
//! and the reason it cross-compiles without a C toolchain — cpal reaches WASAPI
//! through the `windows` crate, which is generated bindings rather than a C
//! library to link.

pub mod capture;
pub mod devices;
pub mod render;
pub mod tone;
pub mod virtual_device;
pub mod wav;

pub use capture::{start_capture, Capture, CaptureConfig};
pub use devices::{describe_default_devices, list_devices, Direction};
pub use render::{OutputTarget, Render, RenderConfig};
pub use tone::Tone;
pub use virtual_device::{detect as detect_mic_routes, MicRoute, Pairing};
pub use wav::WavSink;

/// The only sample format this program asks a device for.
///
/// Both CoreAudio and WASAPI in shared mode hand out `f32`, so requesting
/// anything else would add a conversion path that never runs. A device that
/// cannot do `f32` fails with a clear message instead of being silently
/// misinterpreted.
pub const SAMPLE_FORMAT: cpal::SampleFormat = cpal::SampleFormat::F32;
