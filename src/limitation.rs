//! what made a reading less reliable, as data a caller can match on and a sentence a person
//! is shown

use std::fmt;

/// Something that made a reading less reliable.
///
/// `Display` writes it as the sentence a person is shown.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[non_exhaustive]
pub enum Limitation {
  /// Analysis that ran out of time and covers only the part read by then.
  OutOfTime,
  /// Codec grid scan the analysis ran out of time for.
  GridScanOutOfTime,
  /// Codec grid scan that did not run, its stretches not all read.
  GridScanIncomplete,
  /// Codec grid not scanned in a file that does not state its length.
  GridNotScanned,
  /// File that states a longer length than its bytes can hold.
  ///
  /// The length the bytes hold is used.
  LengthPastBytes,
  /// File that states no length, read no further than the longest file read in full.
  LengthNotStated {
    /// Seconds the analysis stopped after.
    seconds: u64,
  },
  /// File that runs longer than it states, read no further than the longest read in full.
  LongerThanStated {
    /// Seconds the analysis stopped after.
    seconds: u64,
  },
  /// Packets that could not be decoded and were skipped.
  PacketsSkipped {
    /// Number of packets skipped.
    count: u32,
  },
  /// Reading or seeking that stopped before the end of the file, with why in words.
  Decoder(String),
  /// Long file analyzed only at windows spread over it.
  Sampled {
    /// Number of one-second windows analyzed.
    windows: usize,
  },
  /// File with more than two channels, of which only the first two were analyzed.
  ChannelsLeftOut {
    /// Number of channels in the file.
    channels: u32,
  },
  /// Too little audio above silence for a reliable reading.
  LittleAudio {
    /// Length of the audio above silence, in seconds.
    seconds: f64,
  },
  /// Quiet audio, whose upper band is harder to measure.
  Quiet,
  /// Cutoff where audio recorded at an older rate and converted stops too.
  OtherRate {
    /// Rate and who used it.
    rate: String,
    /// Top of that rate's band, in Hz.
    hz: f64,
  },
  /// Cutoff where a recording filtered at 20 kHz stops too.
  FilteredAt20Khz,
  /// Cutoff that shows only the lowest of several lossy generations.
  LowestCutoffOnly,
  /// AAC grid, which a Vorbis decode's grid reads as too.
  VorbisReadsAsAac,
  /// Cutoff at an encoder's without a frame grid.
  ///
  /// A recording filtered there and a decode resampled afterwards both look like this.
  CutoffWithoutGrid,
  /// Early roll-off, as old masters, speech and transfers have by nature.
  EarlyRollOff,
  /// DSD file, measured as PCM filtered from its one-bit samples.
  ///
  /// The noise the one-bit samples carry above the audio band is in the PCM too, where it
  /// reads as content. No codec grid is looked for.
  Dsd {
    /// Rate of the PCM the file was measured as, in Hz.
    pcm_rate: u32,
  },
  /// Upper band an HE-AAC decoder rebuilt from the lower one.
  ///
  /// It holds none of the source's waveform, and where it stops is not an encoder's lowpass.
  RebuiltBand {
    /// Top of the band the core can code, in Hz.
    hz: f64,
  },
}

impl fmt::Display for Limitation {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    match self {
      Self::OutOfTime => {
        f.write_str("The analysis ran out of time and covers only the part read by then")
      }
      Self::GridScanOutOfTime => {
        f.write_str("The analysis ran out of time before the codec grid scan finished")
      }
      Self::GridScanIncomplete => f.write_str(
        "The stretches the codec grid scan reads were not all read, so the scan did not run",
      ),
      Self::GridNotScanned => {
        f.write_str("The file does not state its length, so its codec grid was not scanned")
      }
      Self::LengthPastBytes => f.write_str(
        "The file states a longer length than its bytes can hold, so the length they hold is used",
      ),
      Self::LengthNotStated { seconds } => write!(
        f,
        "The file does not state its length, so the analysis stopped after {}",
        minutes(*seconds)
      ),
      Self::LongerThanStated { seconds } => write!(
        f,
        "The file runs longer than it states, so the analysis stopped after {}",
        minutes(*seconds)
      ),
      Self::PacketsSkipped { count } => {
        write!(f, "{count} packets could not be decoded and were skipped")
      }
      // the decoder's reason is a clause it shares with other messages; here it is a list
      // item of its own
      Self::Decoder(why) => {
        let mut chars = why.chars();
        match chars.next() {
          Some(first) => write!(f, "{}{}", first.to_uppercase(), chars.as_str()),
          None => Ok(()),
        }
      }
      Self::Sampled { windows } => write!(
        f,
        "Only {windows} one-second windows spread over the file were analyzed"
      ),
      Self::ChannelsLeftOut { channels } => {
        write!(f, "Only the first two of {channels} channels were analyzed")
      }
      Self::LittleAudio { seconds } => write!(
        f,
        "Only {seconds:.1} s of the audio is above silence, too little for a reliable reading"
      ),
      Self::Quiet => {
        f.write_str("The audio is quiet, which makes the upper band harder to measure")
      }
      Self::OtherRate { rate, hz } => write!(
        f,
        "Audio recorded at {rate} and converted to this rate has the same sharp cutoff at {}",
        khz(*hz)
      ),
      Self::FilteredAt20Khz => f.write_str(
        "A recording filtered at 20 kHz when it was made looks the same as a high-bitrate \
         encoder's cutoff",
      ),
      Self::LowestCutoffOnly => {
        f.write_str("Of several lossy generations, only the lowest cutoff shows")
      }
      Self::VorbisReadsAsAac => f.write_str("A Vorbis decode's frame grid reads as AAC's"),
      Self::CutoffWithoutGrid => f.write_str(
        "A recording filtered where an encoder cuts, and a decode resampled afterwards, stop at \
         an encoder's cutoff without a frame grid",
      ),
      Self::Dsd { pcm_rate } => write!(
        f,
        "The file is DSD, measured as PCM at {}: the noise its one-bit samples carry above \
         the audio band reads as content, and no codec grid is looked for",
        khz(f64::from(*pcm_rate))
      ),
      Self::RebuiltBand { hz } => write!(
        f,
        "The decoder rebuilt the band above the HE-AAC core, which ends at or under {}, from \
         the band below, so the cutoff is not where an encoder's lowpass sits",
        khz(*hz)
      ),
      Self::EarlyRollOff => f.write_str(
        "Old masters, speech, lo-fi material and vinyl or cassette transfers roll off early, and \
         heavy equalization can look like a shelf",
      ),
    }
  }
}

/// `seconds` in words, as minutes when they are whole minutes
fn minutes(seconds: u64) -> String {
  match seconds {
    60 => "a minute".to_string(),
    s if s > 0 && s % 60 == 0 => format!("{} minutes", s / 60),
    s => format!("{s} seconds"),
  }
}

/// a frequency as kHz to one decimal, without a trailing `.0`
pub(crate) fn khz(hz: f64) -> String {
  let text = format!("{:.1}", hz / 1000.0);
  format!("{} kHz", text.strip_suffix(".0").unwrap_or(&text))
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn a_decoders_clause_is_written_as_a_sentence_of_its_own() {
    let stopped = Limitation::Decoder("seeking failed: end of stream".to_string());
    assert_eq!(stopped.to_string(), "Seeking failed: end of stream");
    assert_eq!(Limitation::Decoder(String::new()).to_string(), "");
    let whole = Limitation::LengthNotStated { seconds: 120 };
    assert!(whole.to_string().ends_with("after 2 minutes"));
  }
}
