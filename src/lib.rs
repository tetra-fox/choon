//! Audio analysis that tells whether a file is what its format claims.
//!
//! A FLAC that is really a converted MP3 is one such file. The analyzer judges from the file's
//! spectrum.
//!
//! # Examples
//!
//! ```no_run
//! use std::path::Path;
//! use std::time::{Duration, Instant};
//!
//! use earwitness::engine::{self, Settings};
//! use earwitness::{MeasureThresholds, ReadingThresholds, reading};
//!
//! # fn main() -> Result<(), earwitness::engine::Canceled> {
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
//! waveform show of it ([`picture`]). [`reading::read`] turns the measures into a [`Reading`]:
//! a [`Verdict`] with how sure it is, the encoder setting the cutoff matches, findings beside it
//! ([`Flag`]), a summary in words and what made it less reliable.
//!
//! # Thresholds
//!
//! Every threshold either step uses is data ([`MeasureThresholds`], [`ReadingThresholds`]), so
//! the caller can sweep them. `Default` holds the values the analyzer ships with.
//!
//! # Features
//!
//! `cli`, on by default, builds the `earwitness` program, which prints a reading of each file it
//! is given, as a line of text or of JSON, under one or more sets of thresholds. A library user
//! turns default features off. `testing` adds `fixtures`, the generated audio and the sample file
//! the analyzer is tested on.

pub mod aggregate;
pub mod decode;
pub mod engine;
pub mod flags;
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
pub use flags::Flag;
pub use reading::{EncoderGuess, Reading, Verdict};
pub use thresholds::{MeasureThresholds, Profile, RateEdge, ReadingThresholds};
