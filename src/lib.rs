//! Audio analysis that tells whether a file is what its format claims.
//!
//! A FLAC that is really a converted MP3 is one such file. The analyzer judges from the file's
//! spectrum and from the frame grid an MP3 or AAC encoder leaves in decoded samples.
//!
//! # Examples
//!
//! ```no_run
//! use std::path::Path;
//! use std::time::{Duration, Instant};
//!
//! use choon::engine::{self, Settings};
//! use choon::{MeasureThresholds, ReadingThresholds, reading};
//!
//! # fn main() -> Result<(), choon::engine::Canceled> {
//! let settings = Settings {
//!   max_full_seconds: 900.0,
//!   sampled_windows: 240,
//!   deadline: Instant::now() + Duration::from_secs(120),
//!   progress: None,
//!   canceled: None,
//!   thresholds: MeasureThresholds::default(),
//! };
//! let measured = engine::run(Path::new("track.flac"), &settings)?;
//! let reading = reading::read(&measured, &ReadingThresholds::default());
//! println!("{:?}: {}", reading.verdict, reading.summary);
//! # Ok(())
//! # }
//! ```
//!
//! # Measuring and reading
//!
//! [`engine::run`] decodes a file a second at a time, in full or at windows spread over a long
//! one, and measures each window: its loudness, where its spectrum stops (the cutoff) and how
//! sharply, gaps below the cutoff, steps inside a full band, and what a spectrogram and a
//! waveform show of it ([`picture`]). In a file that claims a lossless format it also scans four
//! stretches of the samples for a codec's frame grid ([`grid`]). [`reading::read`] turns the
//! measures into a [`Reading`]: a [`Verdict`] with how sure it is, the codec grid found, the
//! encoder setting the cutoff matches, findings beside it ([`Flag`]), a summary in words and
//! what made it less reliable. Only a grid makes a lossless file a likely transcode; a cutoff
//! that reads as lossy without one makes it possibly lossy.
//!
//! # Thresholds
//!
//! Every threshold either step uses is data ([`MeasureThresholds`], [`ReadingThresholds`]), so
//! the caller can sweep them. `Default` holds the values the analyzer ships with.
//!
//! # Features
//!
//! `cli`, on by default, builds the `choon` program, which prints a reading of each file it
//! is given, as a line of text or of JSON, under one or more sets of thresholds. A library user
//! turns default features off. `testing` adds `fixtures`, the generated audio and the sample file
//! the analyzer is tested on.

pub mod aggregate;
mod decode;
pub mod engine;
pub mod flags;
pub mod grid;
pub mod measure;
pub mod picture;
pub mod reading;
pub mod thresholds;
mod totals;

#[cfg(any(test, feature = "testing"))]
pub mod fixtures;
#[cfg(test)]
mod tests;

#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctest;

pub use aggregate::Measures;
pub use decode::Claimed;
pub use flags::Flag;
pub use reading::{EncoderGuess, Reading, Verdict};
pub use thresholds::{MeasureThresholds, Profile, RateEdge, ReadingThresholds};
