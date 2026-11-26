//! Audio analysis that tells whether a file is what its format claims.
//!
//! The usual case is a FLAC made from an MP3. The analyzer judges from the file's spectrum and
//! from the frame grid an MP3, MP2, AAC, Vorbis, WMA, AC-3 or Opus encoder leaves in decoded
//! samples.
//!
//! # Examples
//!
//! ```no_run
//! use choon::{ReadingThresholds, Settings};
//!
//! # fn main() -> Result<(), choon::Error> {
//! let measured = choon::measure("track.flac", &Settings::default())?;
//! let reading = measured.read(&ReadingThresholds::default());
//! println!("{:?}: {}", reading.verdict, reading.summary);
//! # Ok(())
//! # }
//! ```
//!
//! # Measuring and reading
//!
//! [`measure`] decodes a file a second at a time, in full or at windows spread over a long
//! one, and measures each window:
//!
//! - its loudness
//! - where its spectrum stops (the cutoff) and how sharply
//! - gaps below the cutoff
//! - steps inside a full band
//! - what a spectrogram and a waveform show of it ([`picture`])
//!
//! In a file that claims a lossless format it also scans four stretches of the samples for a
//! codec's frame grid ([`grid`]). Of an Opus file it keeps the coding mode and audio bandwidth
//! each packet states ([`OpusUse`]), and of an HE-AAC file what its decoder rebuilt ([`HeAac`]).
//!
//! [`Measured::read`] turns the measures into a [`Reading`]:
//!
//! - a [`Verdict`] with how sure it is
//! - the codec grid found
//! - the encoder setting the cutoff matches
//! - findings beside it ([`Flag`])
//! - a summary in words
//! - what made it less reliable ([`Limitation`])
//!
//! Only a grid makes a lossless file a likely transcode; a cutoff that reads as lossy without
//! one makes it possibly lossy.
//!
//! # Thresholds
//!
//! Every threshold either step uses is data the caller passes ([`MeasureThresholds`],
//! [`ReadingThresholds`]), so a sweep needs no rebuild. `Default` holds the values the analyzer
//! ships with. The cutoffs come from what common encoders do, the grid thresholds were tuned on
//! decodes of known history, and both are checked against generated audio in the tests. They
//! are rules of thumb that real files can break, so a reading is evidence.
//!
//! # Features
//!
//! - `cli`, on by default, builds the `choon` program, which prints a reading of each file it
//!   is given, as a line of text or of JSON, under one or more sets of thresholds. A library
//!   user turns default features off.
//! - `serde` derives `Serialize` and `Deserialize` on the thresholds and on [`Measured`], and
//!   `Serialize` on [`Reading`].
//! - `testing` adds `fixtures`, the generated audio and the sample files the analyzer is tested
//!   on.

mod aggregate;
mod decode;
mod engine;
mod flags;
pub mod grid;
mod kaiser;
mod limitation;
pub mod picture;
mod reading;
mod thresholds;
mod totals;
mod window;

#[cfg(any(test, feature = "testing"))]
pub mod fixtures;
#[cfg(test)]
mod tests;

#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctest;

pub use aggregate::{CutoffBucket, Measures};
pub use decode::opus::{OpusBandwidth, OpusMode, OpusUse};
pub use decode::{Claimed, HeAac};
pub use engine::{CancelCheck, Coverage, Error, Measured, Settings, measure, measure_bytes};
pub use flags::Flag;
pub use limitation::Limitation;
pub use reading::{EncoderGuess, Reading, Verdict};
pub use thresholds::{
  EncoderProfile, FamilyProfile, FamilySteps, MeasureThresholds, RateEdge, ReadingThresholds,
  StepBound, StepRule, StepShare,
};
pub use window::{Step, Window};
