//! The analyzer's reading of its measures: the cutoff read from its spread over the windows,
//! weighed with the grid scan, with the findings beside it.

use crate::aggregate::{Measures, measures, middle, percentile, upsample_ratio};
use crate::engine::Measured;
use crate::flags::{Flag, findings};
use crate::grid::{self, Codec, Found};
use crate::limitation::{Limitation, khz};
use crate::thresholds::{EncoderProfile, ReadingThresholds};
use crate::window::{Step, Window};

/// The top of the band of a 48 kHz recording; a file whose band goes past it is hi-res.
const HI_RES_TOP_HZ: f64 = 24_000.0;

/// What the analyzer makes of a file.
///
/// # Examples
///
/// ```
/// use choon::{ReadingThresholds, Verdict};
///
/// # fn main() -> Result<(), choon::Error> {
/// # let tone = choon::fixtures::TONE.to_vec();
/// # let measured = choon::measure_bytes(tone, &choon::Settings::default())?;
/// let reading = measured.read(&ReadingThresholds::default());
/// let lossy = match reading.verdict {
///   Verdict::LikelyTranscode | Verdict::MixedSources => true,
///   Verdict::Consistent | Verdict::PossiblyLossy | Verdict::Inconclusive => false,
/// };
/// println!("{}", if lossy { "lossy" } else { "no lossy source found" });
/// # Ok(())
/// # }
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Verdict {
  /// Nearly every window reaches the top of the claimed band.
  ///
  /// It does so without an encoder's shelf.
  Consistent,
  /// A codec's frame grid shows in the samples.
  ///
  /// Where no grid scan ran, as in a file that claims a lossy codec, nearly every window stops
  /// at the same known encoder cutoff with a sharp shelf.
  LikelyTranscode,
  /// Part of the time or of the band is band-limited while the rest is full band.
  ///
  /// It is what one low-bitrate stem mixed into a lossless track looks like. A file whose grid
  /// was scanned reads as possibly lossy instead.
  MixedSources,
  /// The cutoff reads as lossy, but no codec's frame grid shows.
  ///
  /// The cutoff alone reads as a transcode or as mixed sources. A recording filtered where an
  /// encoder cuts and a decode resampled afterwards both look like this.
  PossiblyLossy,
  /// Too little audio, or measures that fit no pattern.
  Inconclusive,
}

/// Encoder setting a cutoff matches.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
pub struct EncoderGuess {
  /// Setting's name, as its profile gives it.
  pub profile: String,
  /// How sure the analyzer is of the match, from 0 to 1.
  pub confidence: f64,
}

/// What the analyzer makes of a file.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
pub struct Reading {
  /// Reading of the file, the cutoff weighed with the grid scan.
  pub verdict: Verdict,
  /// Reading of the cutoff alone.
  pub cutoff_verdict: Verdict,
  /// Codec frame grid the samples show.
  pub grid: Option<Found>,
  /// How sure the reading is, from 0 to the confidence cap.
  pub confidence: f64,
  /// Findings independent of the cutoff.
  pub flags: Vec<Flag>,
  /// Encoder setting the cutoff matches.
  pub encoder_guess: Option<EncoderGuess>,
  /// What the reading found, in words.
  pub summary: String,
  /// Aggregate measures.
  pub measures: Measures,
  /// What made the reading less reliable.
  #[cfg_attr(feature = "serde", serde(serialize_with = "sentences"))]
  pub limitations: Vec<Limitation>,
  /// Cutoff to mark on the spectrogram, when the content stops short of the top.
  pub cutoff_hz: Option<f64>,
}

/// The reading of the cutoff alone, before the findings independent of it are added.
struct CutoffReading {
  verdict: Verdict,
  confidence: f64,
  encoder_guess: Option<EncoderGuess>,
  summary: String,
  limitations: Vec<Limitation>,
  cutoff_hz: Option<f64>,
}

/// Writes limitations as the sentences a person is shown.
#[cfg(feature = "serde")]
fn sentences<S: serde::Serializer>(
  limitations: &[Limitation],
  serializer: S,
) -> Result<S::Ok, S::Error> {
  serializer.collect_seq(limitations.iter().map(ToString::to_string))
}

/// A window that is not near-silent, as the reading counts it.
#[derive(Clone, Copy, Debug)]
struct Seen {
  cutoff_hz: Option<f64>,
  /// it stops with an encoder's low-pass rather than a natural roll-off
  sharp: bool,
  seconds: f64,
  inner_shelf: Option<Step>,
  rms_dbfs: f64,
}

impl Seen {
  /// `window` as the reading counts it against a band ending at `top`. A window whose band
  /// reaches the top only through a floor that just clears the cutoff line, above a step at an
  /// encoder's cutoff, stops at that step: hiss or dither over a filtered recording is not
  /// full-band content.
  fn of(window: &Window, top: f64, thresholds: &ReadingThresholds) -> Self {
    let floor = window.inner_shelf.filter(|step| {
      window
        .cutoff_hz
        .is_some_and(|c| c >= thresholds.full_band * top)
        && step.drop >= thresholds.inner_shelf_db
        && thresholds.profile_at(step.hz).is_some()
        && step.clearance < thresholds.clear_db
    });
    match floor {
      Some(step) => Self {
        cutoff_hz: Some(step.hz),
        // a drop of this size to the floor within a few hundred hertz is a wall, not a
        // roll-off, whatever the floor's level makes the number
        sharp: true,
        inner_shelf: None,
        rms_dbfs: window.rms_dbfs,
        seconds: window.seconds,
      },
      None => Self {
        cutoff_hz: window.cutoff_hz,
        sharp: window.sharp(thresholds),
        inner_shelf: window.inner_shelf,
        rms_dbfs: window.rms_dbfs,
        seconds: window.seconds,
      },
    }
  }
}

/// The windows that are not near-silent, as the reading counts them.
struct Heard<'a> {
  windows: Vec<Seen>,
  /// the top of the band the content is judged against, in Hz
  top: f64,
  thresholds: &'a ReadingThresholds,
}

impl<'a> Heard<'a> {
  /// How many seconds of audio the windows hold, a short last one by its length.
  fn seconds(&self) -> f64 {
    self.windows.iter().map(|w| w.seconds).sum()
  }

  fn share(&self, keep: impl Fn(&Seen) -> bool) -> f64 {
    let kept = self.windows.iter().filter(|w| keep(w)).count();
    kept as f64 / self.windows.len().max(1) as f64
  }

  fn full(&self, window: &Seen) -> bool {
    window
      .cutoff_hz
      .is_some_and(|c| c >= self.thresholds.full_band * self.top)
  }

  /// A window that stops short of the top at an encoder's cutoff, with a sharp shelf.
  fn limited(&self, window: &Seen) -> Option<&'a EncoderProfile> {
    let cutoff = window.cutoff_hz?;
    (!self.full(window) && window.sharp)
      .then(|| lower_profile_at(self.thresholds, cutoff))
      .flatten()
  }

  /// Where a full-band window has a step of `depth` dB or more under its top.
  fn stepped(&self, window: &Seen, depth: f64) -> Option<f64> {
    let step = window.inner_shelf?;
    (self.full(window) && step.drop >= depth).then_some(step.hz)
  }

  /// A full-band window with a band-limited layer under it at an encoder's cutoff.
  fn layered(&self, window: &Seen) -> Option<f64> {
    self
      .stepped(window, self.thresholds.inner_shelf_db)
      .filter(|hz| lower_profile_at(self.thresholds, *hz).is_some())
  }
}

/// The profile at `hz` when it is one below the highest bitrates: a cut where those cut is
/// also where recordings are filtered, so it is no sign of a lossy part in a mix either.
fn lower_profile_at(thresholds: &ReadingThresholds, hz: f64) -> Option<&EncoderProfile> {
  thresholds
    .profile_at(hz)
    .filter(|p| p.kbps < thresholds.high_bitrate_kbps)
}

impl Measured {
  /// Reads the measures with `thresholds`.
  ///
  /// A reading says whether the audio is consistent with the format it claims, a likely
  /// transcode, a mix of sources, possibly lossy, or something the analyzer cannot tell, with
  /// how sure it is, the findings independent of that, one summary, and what made it less
  /// reliable. The cutoff is read from the spread over windows, not an average, so a
  /// band-limited minority of the time or of the band reads as mixed sources rather than as a
  /// transcode.
  ///
  /// In a file that claims a lossless format, the cutoff is weighed with the scan for a
  /// codec's frame grid ([`grid`]). Only a grid calls the file a likely transcode; a cutoff
  /// that reads as lossy without one makes the file possibly lossy.
  ///
  /// # Examples
  ///
  /// Thresholds can come from JSON, as a profile; keys left out keep their default:
  ///
  /// ```
  /// use choon::ReadingThresholds;
  ///
  /// # fn main() -> Result<(), Box<dyn std::error::Error>> {
  /// # let tone = choon::fixtures::TONE.to_vec();
  /// # let measured = choon::measure_bytes(tone, &choon::Settings::default())?;
  /// let thresholds: ReadingThresholds = serde_json::from_str(r#"{"sharpShelfDb": 25.0}"#)?;
  /// let reading = measured.read(&thresholds);
  /// println!("{:?}, {}", reading.verdict, reading.summary);
  /// # Ok(())
  /// # }
  /// ```
  pub fn read(&self, thresholds: &ReadingThresholds) -> Reading {
    let claimed = &self.claimed;
    let nyquist = f64::from(claimed.sample_rate) / 2.0;
    let windows: Vec<&Window> = self
      .windows
      .iter()
      .filter(|w| w.cutoff_hz.is_some())
      .collect();
    let upsample_ratio = (claimed.sample_rate > 48_000).then(|| upsample_ratio(&windows));
    let measures = measures(self, &windows, upsample_ratio);
    let median = measures.cutoff_median_hz.map(f64::from);
    let found_beside = findings(claimed, &windows, median, &measures, thresholds);
    // an upsampled file is judged against the band of the rate it was made at
    let top = found_beside.source_edge.unwrap_or(nyquist);
    let windows: Vec<Seen> = windows
      .iter()
      .map(|w| Seen::of(w, top, thresholds))
      .collect();
    let cutoffs: Vec<f64> = windows.iter().filter_map(|w| w.cutoff_hz).collect();
    let median = (!cutoffs.is_empty()).then(|| middle(&cutoffs));
    let heard = Heard {
      windows,
      top,
      thresholds,
    };
    let mut limitations = self.problems.clone();
    if self.sampled {
      limitations.push(Limitation::Sampled {
        windows: self.windows.len(),
      });
    }
    if self.channels_left_out > 0 {
      limitations.push(Limitation::ChannelsLeftOut {
        channels: claimed.channels,
      });
    }
    let mut found = verdict(&heard, median, claimed.lossless, self.bitrate_kbps);
    let heard_seconds = heard.seconds();
    if heard_seconds < thresholds.few_seconds() {
      limitations.push(Limitation::LittleAudio {
        seconds: heard_seconds,
      });
    }
    if quiet(&heard) {
      limitations.push(Limitation::Quiet);
    }
    // a lossy file read as consistent with its own bitrate stops where its own encoder cuts,
    // which needs no other explanation
    let own_cutoff = !claimed.lossless && found.verdict == Verdict::Consistent;
    if let Some(edge) = found_beside.source_edge
      && let Some(profile) = thresholds.profile_at(edge)
      && found.verdict != Verdict::MixedSources
      && !own_cutoff
    {
      // the edge of the lower rate is also where an encoder cuts: either could be the source
      found = CutoffReading {
        verdict: Verdict::Inconclusive,
        confidence: 0.4,
        encoder_guess: Some(EncoderGuess {
          profile: profile.name.clone(),
          confidence: 0.4,
        }),
        summary: format!(
          "The content stops sharply at {}, both the upper limit of a {} kHz recording and the \
         cutoff of an encoder at {}. The file was either upsampled from that rate or \
         transcoded from that encoder, and the analyzer cannot tell which.",
          khz(edge),
          format!("{:.2}", edge * 2.0 / 1000.0)
            .trim_end_matches('0')
            .trim_end_matches('.'),
          profile.name
        ),
        limitations: Vec::new(),
        cutoff_hz: Some(edge),
      };
    }
    if found.verdict != Verdict::Consistent
      && let Some(cut) = found.cutoff_hz
      && let Some(edge) = thresholds
        .other_rate_edges
        .iter()
        .find(|edge| (cut - edge.hz).abs() <= thresholds.cluster_hz)
    {
      limitations.push(Limitation::OtherRate {
        rate: edge.rate.clone(),
        hz: edge.hz,
      });
    }
    let cutoff_verdict = found.verdict;
    let grid = self
      .grid
      .as_ref()
      .and_then(|scan| grid::found(scan, thresholds));
    if self.grid.is_some() {
      found = weighed(found, grid.as_ref(), thresholds);
    }
    limitations.extend(found.limitations);
    Reading {
      verdict: found.verdict,
      cutoff_verdict,
      grid,
      confidence: found.confidence.min(thresholds.confidence_cap),
      flags: found_beside.flags,
      encoder_guess: found.encoder_guess,
      summary: std::iter::once(found.summary)
        .chain(found_beside.notes)
        .collect::<Vec<_>>()
        .join(" "),
      measures,
      limitations,
      cutoff_hz: found.cutoff_hz,
    }
  }
}

/// The cutoff reading `found` weighed with the grid scan, which found `grid`: a grid makes a
/// likely transcode, and a cutoff that reads as lossy without one makes a doubt.
fn weighed(
  found: CutoffReading,
  grid: Option<&Found>,
  thresholds: &ReadingThresholds,
) -> CutoffReading {
  if let Some(grid) = grid {
    let name = grid.codec;
    let mut limitations = found.limitations;
    if grid.codec == Codec::Aac {
      limitations.push(Limitation::VorbisReadsAsAac);
    }
    return CutoffReading {
      verdict: Verdict::LikelyTranscode,
      confidence: thresholds.confidence_cap,
      summary: format!(
        "The samples carry an {name} frame grid at offset {} (z {:.0}, {} of {} segments \
         agreeing), as audio decoded from {name} does. {}",
        grid.scan.offset,
        grid.scan.z,
        grid.scan.segments_agreeing,
        grid::SEGMENTS,
        found.summary
      ),
      limitations,
      ..found
    };
  }
  match found.verdict {
    Verdict::LikelyTranscode | Verdict::MixedSources => {
      let mut limitations = found.limitations;
      limitations.push(Limitation::CutoffWithoutGrid);
      CutoffReading {
        verdict: Verdict::PossiblyLossy,
        confidence: 0.4,
        summary: format!(
          "{} No MP3 or AAC frame grid shows in the samples, so the cutoff alone cannot confirm \
           a lossy source.",
          found.summary
        ),
        limitations,
        ..found
      }
    }
    Verdict::Consistent | Verdict::PossiblyLossy | Verdict::Inconclusive => found,
  }
}

/// Reads the cutoff of the non-silent windows, whose median cutoff is `median`.
fn verdict(
  heard: &Heard,
  median: Option<f64>,
  lossless: bool,
  bitrate: Option<u32>,
) -> CutoffReading {
  let t = heard.thresholds;
  let count = heard.windows.len();
  let seconds = heard.seconds();
  let mut reading = CutoffReading {
    verdict: Verdict::Inconclusive,
    confidence: 0.3,
    encoder_guess: None,
    summary: String::new(),
    limitations: Vec::new(),
    cutoff_hz: median.filter(|m| *m < t.full_band * heard.top),
  };
  let Some(median) = median.filter(|_| count >= t.min_windows && seconds >= t.min_seconds) else {
    reading.confidence = 0.2;
    reading.summary = format!(
      "Only {seconds:.1} s of the audio is above silence; a reading needs at least {:.0} s.",
      t.min_seconds
    );
    return reading;
  };
  let near = |hz: f64| heard.share(|w| w.cutoff_hz.is_some_and(|c| (c - hz).abs() <= t.cluster_hz));
  let clustered = near(median);
  let sharp_share = heard.share(|w| {
    w.sharp
      && w
        .cutoff_hz
        .is_some_and(|c| (c - median).abs() <= t.cluster_hz)
  }) / clustered.max(f64::EPSILON);
  // a lossy file's own encoder cuts off too: only a cutoff well below its bitrate's counts
  let below_claim = |profile: &EncoderProfile| {
    lossless || bitrate.is_some_and(|kbps| below_bitrate(profile.kbps, kbps))
  };

  if let Some(profile) = lower_profile_at(t, median).filter(|p| below_claim(p))
    && clustered >= t.transcode_share
    && sharp_share >= 0.5
  {
    let evidence = 0.6
      * ((clustered - t.transcode_share) / (1.0 - t.transcode_share)).clamp(0.0, 1.0)
      + 0.4 * sharp_share;
    let confidence = 0.6 + 0.38 * evidence;
    reading.limitations.push(Limitation::LowestCutoffOnly);
    reading.verdict = Verdict::LikelyTranscode;
    reading.confidence = confidence;
    reading.encoder_guess = Some(EncoderGuess {
      profile: profile.name.clone(),
      confidence: (clustered * (0.5 + 0.5 * sharp_share)).min(t.confidence_cap),
    });
    reading.cutoff_hz = Some(median);
    reading.summary = format!(
      "{} of the time stops at {} with a sharp shelf, where an encoder at {} cuts off.",
      percent(clustered),
      khz(median),
      profile.name
    );
    return reading;
  }

  if let Some(profile) = t.profile_at(median).filter(|_| lossless)
    && clustered >= t.transcode_share
    && sharp_share >= 0.5
  {
    reading.cutoff_hz = Some(median);
    // past the transcode rule, this is a sharp cutoff where the highest bitrates cut
    reading.encoder_guess = Some(EncoderGuess {
      profile: profile.name.clone(),
      confidence: 0.4,
    });
    reading.limitations.push(Limitation::FilteredAt20Khz);
    reading.summary = format!(
      "{} of the time stops at {} with a sharp shelf, where an encoder at {} cuts off or where \
       a recording was filtered when it was made.",
      percent(clustered),
      khz(median),
      profile.name
    );
    return reading;
  }

  let full = heard.share(|w| heard.full(w));
  let limited: Vec<f64> = heard
    .windows
    .iter()
    .filter(|w| heard.limited(w).is_some())
    .filter_map(|w| w.cutoff_hz)
    .collect();
  let limited_share = limited.len() as f64 / count as f64;
  if limited_share >= t.mixed_share && full >= t.mixed_share {
    let at = middle(&limited);
    reading.verdict = Verdict::MixedSources;
    reading.confidence = 0.5 + 0.35 * (limited_share.min(full) / 0.5).min(1.0);
    reading.encoder_guess = t.profile_at(at).map(|p| EncoderGuess {
      profile: p.name.clone(),
      confidence: 0.5,
    });
    reading.cutoff_hz = Some(at);
    reading.summary = format!(
      "{} of the time is full-band; {} stops at {} with a sharp shelf, as when part of the \
       track comes from a lossy source.",
      percent(full),
      percent(limited_share),
      khz(at)
    );
    return reading;
  }
  let layers: Vec<f64> = heard
    .windows
    .iter()
    .filter_map(|w| heard.layered(w))
    .collect();
  let layered_share = layers.len() as f64 / count as f64;
  if layered_share >= t.mixed_share {
    let at = middle(&layers);
    reading.verdict = Verdict::MixedSources;
    reading.confidence = 0.5 + 0.35 * layered_share.min(1.0);
    reading.encoder_guess = t.profile_at(at).map(|p| EncoderGuess {
      profile: p.name.clone(),
      confidence: 0.5,
    });
    reading.cutoff_hz = Some(at);
    reading.summary = format!(
      "The content reaches the top of the band, but {} of the time has a sharp step at {}, as \
       when a band-limited layer is mixed with full-band material.",
      percent(layered_share),
      khz(at)
    );
    return reading;
  }
  // a step where the highest bitrates cut, under a band that reaches the top: a high-bitrate
  // layer, or a recording filtered at 20 kHz with something above it
  let high_steps: Vec<f64> = heard
    .windows
    .iter()
    .filter_map(|w| heard.stepped(w, t.high_step_db))
    .filter(|hz| {
      t.profile_at(*hz)
        .is_some_and(|p| p.kbps >= t.high_bitrate_kbps)
    })
    .collect();
  if high_steps.len() as f64 / count as f64 >= t.mixed_share {
    let at = middle(&high_steps);
    // every step here is at a profile
    let profile = t
      .profile_at(at)
      .map_or("a high bitrate", |p| p.name.as_str());
    reading.encoder_guess = Some(EncoderGuess {
      profile: profile.to_string(),
      confidence: 0.4,
    });
    reading.cutoff_hz = Some(at);
    reading.limitations.push(Limitation::FilteredAt20Khz);
    reading.summary = format!(
      "The content reaches the top of the band, but {} of the time has a sharp step at {}, \
       where an encoder at {profile} cuts off. A layer at that bitrate and a recording \
       filtered there with other material above it look the same.",
      percent(high_steps.len() as f64 / count as f64),
      khz(at)
    );
    return reading;
  }

  let open = heard.share(|w| heard.full(w) || !w.sharp);
  if open >= t.consistent_share {
    if full >= t.consistent_share {
      let windows = ((count - t.min_windows) as f64 / 100.0).min(1.0);
      let mut confidence = 0.7 + 0.25 * windows;
      if quiet(heard) {
        confidence -= 0.1;
      }
      reading.verdict = Verdict::Consistent;
      reading.confidence = confidence;
      // a step that is no encoder's cutoff can still lie under the band
      let stepped = heard
        .windows
        .iter()
        .any(|w| heard.stepped(w, t.high_step_db).is_some());
      reading.summary = format!(
        "{} of the time reaches {}, near the top of the {} band{}.",
        percent(full),
        khz(median),
        khz(heard.top),
        if stepped {
          ""
        } else {
          ", with no encoder shelf"
        }
      );
      return reading;
    }
    reading.limitations.push(Limitation::EarlyRollOff);
    if median >= t.natural_limit * heard.top {
      reading.verdict = Verdict::Consistent;
      reading.confidence = 0.7;
      reading.summary = format!(
        "The content rolls off gently from about {}, not at the sharp cutoff of an encoder.",
        khz(median)
      );
      return reading;
    }
    reading.confidence = 0.4;
    // a band above what a 48 kHz recording holds is mostly empty in honest hi-res audio too
    let like = if heard.top > HI_RES_TOP_HZ {
      "as in many hi-res recordings, where microphones and instruments produce little above \
       20 kHz"
    } else {
      "as in test tones, speech, old masters and lo-fi recordings"
    };
    reading.summary = format!(
      "The content stops at about {}, well below the top of the band, without the sharp shelf \
       of an encoder, {like}.",
      khz(median)
    );
    return reading;
  }
  // nothing above says otherwise: a lossy file that reaches past every cutoff too low for its
  // bitrate is judged against what its own bitrate keeps. Its encoder's cutoff is a ceiling that
  // moves from frame to frame, and windows under it only lack high content, so it is judged at
  // the top of its cutoffs
  if !lossless && let Some(kbps) = bitrate {
    let cutoffs: Vec<f64> = heard.windows.iter().filter_map(|w| w.cutoff_hz).collect();
    let edge = percentile(&cutoffs, t.ceiling).unwrap_or(median);
    let too_low = t
      .profiles
      .iter()
      .rev()
      .find(|p| below_claim(p))
      .map_or(0.0, |p| p.hz);
    if edge > too_low + t.cluster_hz {
      reading.cutoff_hz = Some(edge);
      let profile = t.profile_at(edge);
      if keeps_its_bitrate(t, edge, kbps) {
        reading.verdict = Verdict::Consistent;
        reading.confidence = 0.7 + 0.1 * near(edge);
        reading.summary = match profile {
          Some(profile) => format!(
            "The content stops at about {}, where an encoder at {} cuts off, at or above what \
             a file of its bitrate keeps.",
            khz(edge),
            profile.name
          ),
          None => format!(
            "The content reaches about {}, the top of what a file of its bitrate keeps.",
            khz(edge)
          ),
        };
        return reading;
      }
      reading.confidence = 0.4;
      reading.encoder_guess = profile.map(|p| EncoderGuess {
        profile: p.name.clone(),
        confidence: 0.4,
      });
      let at = profile.map_or(String::new(), |p| {
        format!(", where an encoder at {} cuts off", p.name)
      });
      reading.summary = format!(
        "The content stops at about {}{at}: below what a file of its bitrate keeps, but not \
         low enough to call a transcode. A second lossy generation at a nearby bitrate is one \
         explanation.",
        khz(edge)
      );
      return reading;
    }
  }
  reading.summary = if sharp_share >= 0.5 && clustered >= t.transcode_share {
    format!(
      "The content stops at {} with a sharp shelf, which is not the cutoff of a common encoder.",
      khz(median)
    )
  } else {
    "The windows do not fit one pattern.".to_string()
  };
  reading
}

/// Whether a profile at `profile_kbps` cuts well below what a file of `kbps` keeps: the file's
/// bitrate is at least one and a half times the profile's.
fn below_bitrate(profile_kbps: u32, kbps: u32) -> bool {
  u64::from(profile_kbps) * 3 / 2 <= u64::from(kbps)
}

/// Whether a cut at `edge` keeps what a file of `kbps` does: it sits at the cutoff of a bitrate
/// no lower than the highest one up to `kbps`, or past every cutoff of that bitrate. A file below
/// every bitrate known keeps whatever it does.
fn keeps_its_bitrate(thresholds: &ReadingThresholds, edge: f64, kbps: u32) -> bool {
  let profiles = &thresholds.profiles;
  let Some(own) = profiles.iter().map(|p| p.kbps).filter(|p| *p <= kbps).max() else {
    return true;
  };
  match thresholds.profile_at(edge) {
    Some(profile) => profile.kbps >= own,
    None => profiles
      .iter()
      .filter(|p| p.kbps == own)
      .all(|p| edge > p.hz),
  }
}

/// Whether the middle window is quiet.
fn quiet(heard: &Heard) -> bool {
  let levels: Vec<f64> = heard.windows.iter().map(|w| w.rms_dbfs).collect();
  percentile(&levels, 0.5).is_some_and(|level| level < heard.thresholds.quiet_dbfs)
}

/// A share as a whole percentage.
fn percent(share: f64) -> String {
  format!("{:.0}%", share * 100.0)
}

#[cfg(test)]
mod tests {
  use super::*;

  fn heard<'a>(cutoffs: &[f64], thresholds: &'a ReadingThresholds) -> Heard<'a> {
    Heard {
      windows: cutoffs
        .iter()
        .map(|c| Seen {
          cutoff_hz: Some(*c),
          sharp: true,
          seconds: 1.0,
          inner_shelf: None,
          rms_dbfs: -20.0,
        })
        .collect(),
      top: 24_000.0,
      thresholds,
    }
  }

  fn spread(cuts: &[(f64, usize)]) -> Vec<f64> {
    cuts.iter().flat_map(|(c, n)| [*c].repeat(*n)).collect()
  }

  #[test]
  fn a_bitrate_at_the_top_of_its_type_is_compared_without_overflow() {
    assert!(below_bitrate(128, 192));
    assert!(!below_bitrate(128, 191));
    assert!(!below_bitrate(u32::MAX, u32::MAX));
  }

  #[test]
  fn a_moving_cutoff_at_the_top_of_a_256_kbps_aac_is_its_own() {
    let cuts = spread(&[
      (20_000.0, 30),
      (20_500.0, 30),
      (21_000.0, 30),
      (15_500.0, 20),
    ]);
    let found = verdict(
      &heard(&cuts, &ReadingThresholds::default()),
      Some(20_250.0),
      false,
      Some(256),
    );
    assert_eq!(found.verdict, Verdict::Consistent, "{}", found.summary);
  }

  #[test]
  fn a_ceiling_too_low_for_the_bitrate_is_not_called_consistent() {
    let cuts = spread(&[(15_500.0, 40), (16_500.0, 40), (17_500.0, 40)]);
    let found = verdict(
      &heard(&cuts, &ReadingThresholds::default()),
      Some(16_500.0),
      false,
      Some(256),
    );
    assert_ne!(found.verdict, Verdict::Consistent, "{}", found.summary);
  }

  #[test]
  fn a_full_band_minority_too_small_to_mix_does_not_hide_a_transcode() {
    // a 128 kbps source in a 320 kbps MP3 with a full-band intro under the mixed-sources share
    let cuts = spread(&[(16_000.0, 88), (20_000.0, 12)]);
    let found = verdict(
      &heard(&cuts, &ReadingThresholds::default()),
      Some(16_000.0),
      false,
      Some(320),
    );
    assert_eq!(found.verdict, Verdict::LikelyTranscode, "{}", found.summary);
  }

  #[test]
  fn a_cutoff_below_its_bitrates_own_but_above_two_thirds_of_it_is_inconclusive() {
    let cuts = spread(&[(16_000.0, 100)]);
    let found = verdict(
      &heard(&cuts, &ReadingThresholds::default()),
      Some(16_000.0),
      false,
      Some(190),
    );
    assert_eq!(found.verdict, Verdict::Inconclusive, "{}", found.summary);
    assert_eq!(found.confidence, 0.4);
    let guess = found.encoder_guess.unwrap();
    assert_eq!(
      (guess.profile.as_str(), guess.confidence),
      ("mp3 ~128 kbps", 0.4)
    );
    assert!(
      found.summary.contains("second lossy generation"),
      "{}",
      found.summary
    );
    let own = verdict(
      &heard(&spread(&[(18_600.0, 100)]), &ReadingThresholds::default()),
      Some(18_600.0),
      false,
      Some(190),
    );
    assert_eq!(own.verdict, Verdict::Consistent, "{}", own.summary);
  }
}
