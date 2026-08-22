//! Sequencing, jitter buffering, drift correction and resampling.
//!
//! Everything here is pure computation: no sockets, no audio devices, no
//! threads of its own. That is what lets the difficult parts — the parts that
//! only misbehave after twenty minutes on real hardware — be tested in
//! milliseconds against a simulated clock, on any platform, in CI.
//!
//! The receive path is assembled by [`pipeline::build`], which returns the two
//! halves that live on the network thread and in the audio callback
//! respectively. The send path needs only [`ring::frame_channel`] and
//! [`pcm`].

pub mod channels;
pub mod drift;
pub mod gain;
pub mod level;
pub mod pcm;
pub mod pipeline;
pub mod resample;
pub mod ring;
pub mod sequencer;

pub use channels::{ChannelMap, Mapping};
pub use drift::{DriftController, DriftGains};
pub use gain::Gain;
pub use level::LevelMeter;
pub use pipeline::{build, FillReport, MediaSink, PipelineConfig, PipelineStats, PlaybackSource};
pub use resample::VariableResampler;
pub use ring::{frame_channel, FrameConsumer, FrameProducer};
pub use sequencer::{Sequencer, SequencerStats};
