//! a reading in the program's words: the verdict's label, what a line says was found, the
//! encoder, the format. plain text over the library's public types and nothing of a terminal,
//! as `web/src/lib.rs` includes this file for the page to say the same

use choon::grid::{Evidence, Family};
use choon::{Claimed, Flag, HeAac, OpusUse, Reading, Verdict};

/// whether `verdict` says a lossless file holds lossy audio
pub fn lossy(verdict: Verdict) -> bool {
  match verdict {
    Verdict::LikelyTranscode | Verdict::MixedSources => true,
    Verdict::Consistent | Verdict::PossiblyLossy | Verdict::Inconclusive => false,
  }
}

/// whether a terminal lists a file read as `verdict` among its findings
pub fn flagged(verdict: Verdict) -> bool {
  lossy(verdict) || verdict == Verdict::PossiblyLossy
}

/// `verdict` in words
pub fn label(verdict: Verdict) -> &'static str {
  match verdict {
    Verdict::Consistent => "consistent",
    Verdict::LikelyTranscode => "likely transcode",
    Verdict::MixedSources => "mixed sources",
    Verdict::PossiblyLossy => "possibly lossy",
    Verdict::Inconclusive => "inconclusive",
  }
}

/// the encoder a reading names: without a grid the setting its cutoff matches, with one the
/// grid's codec, its encoder family in parentheses and the setting the reading names beside
/// it. `None` where a grid has neither a family nor a setting to name, and beside a verdict
/// that finds no lossy audio, where a named encoder would read as a finding
pub fn encoder(reading: &Reading) -> Option<String> {
  let Some(grid) = &reading.grid else {
    let guess = reading.encoder_guess.as_ref();
    return guess
      .filter(|_| flagged(reading.verdict))
      .map(|guess| guess.profile.to_string());
  };
  let codec = grid.codec;
  let setting = grid.setting.as_deref().filter(|_| flagged(reading.verdict));
  match (grid.family.and_then(family_words), setting) {
    (None, None) => None,
    (None, Some(setting)) => Some(format!("{codec} {setting}")),
    (Some(family), None) => Some(format!("{codec} ({family})")),
    (Some(family), Some(setting)) => Some(format!("{codec} ({family}) {setting}")),
  }
}

/// what a file is beside its codec: what an Opus file's packets state, the profile of an HE-AAC
/// file, the rate of a DSD file
pub fn kind(opus: &[OpusUse], he_aac: Option<HeAac>, dsd_multiple: Option<u32>) -> Option<String> {
  match (OpusUse::describe(opus), he_aac, dsd_multiple) {
    (Some(coding), ..) => Some(format!("Opus {coding}")),
    (None, Some(he_aac), _) => Some(he_aac.to_string()),
    (None, None, Some(multiple)) => Some(format!("DSD{multiple}")),
    (None, None, None) => None,
  }
}

/// what a file claims to be, a few words each: codec, rate, bit depth, channels, bitrate and
/// length. `duration` is in seconds
pub fn format(claimed: &Claimed, bitrate_kbps: Option<u32>, duration: f64) -> String {
  let mut format = vec![
    codec_words(&claimed.codec).to_string(),
    format!("{} Hz", claimed.sample_rate),
  ];
  if let Some(bits) = claimed.bit_depth {
    format.push(format!("{bits} bit"));
  }
  format.push(match claimed.channels {
    1 => "1 channel".to_string(),
    channels => format!("{channels} channels"),
  });
  if let Some(kbps) = bitrate_kbps {
    format.push(format!("{kbps} kbit/s"));
  }
  let seconds = duration.round() as u64;
  format.push(format!("{}:{:02}", seconds / 60, seconds % 60));
  format.join(", ")
}

/// the codec a decoder's short name stands for, spelled as its makers spell it. a name the
/// program has no word for is given as it is
fn codec_words(codec: &str) -> &str {
  match codec {
    "flac" => "FLAC",
    "alac" => "ALAC",
    "mp1" => "MP1",
    "mp2" => "MP2",
    "mp3" => "MP3",
    "aac" => "AAC",
    "opus" => "Opus",
    "vorbis" => "Vorbis",
    "wavpack" => "WavPack",
    "ape" => "Monkey's Audio",
    "dsd" => "DSD",
    pcm if pcm.starts_with("pcm_") => "PCM",
    adpcm if adpcm.starts_with("adpcm_") => "ADPCM",
    other => other,
  }
}

/// a frequency as kHz to one decimal
pub fn khz(hz: f64) -> String {
  format!("{:.1} kHz", hz / 1000.0)
}

/// what a reading found, a few words each: the grid, the cutoff, the encoder, the flags, the
/// confidence. `scanned` is whether the file's grid scan ran
pub fn found(reading: &Reading, scanned: bool) -> Vec<String> {
  let mut found = Vec::new();
  let cutoff = reading.cutoff_hz.map_or_else(
    || "no cutoff".to_string(),
    |hz| format!("cutoff {}", khz(hz)),
  );
  let guess = encoder(reading);
  if let Some(grid) = &reading.grid {
    let codec = grid.codec;
    found.push(match &grid.evidence {
      Evidence::Count(_)
      | Evidence::Steps { .. }
      | Evidence::AacSteps { .. }
      | Evidence::Lattice(_) => format!("{codec} grid"),
      Evidence::CountAt { rate, .. } => {
        format!("{codec} grid at {} kHz", f64::from(*rate) / 1000.0)
      }
      Evidence::Part { segments, scan } => format!(
        "{codec} grid in {segments} of {} stretches",
        scan.segment_z.len()
      ),
      Evidence::Shifted { .. } => format!("{codec} grid at another level"),
      Evidence::Hybrid { .. } => format!("{codec} hybrid grid"),
      Evidence::Unfiltered { .. } => format!("{codec} grid under its pitch filter"),
      Evidence::Short { short } => format!("{codec} grid, {} ms frames", short.milliseconds()),
    });
    found.push(cutoff);
    found.extend(guess);
    return found;
  }
  if reading.verdict == Verdict::PossiblyLossy {
    if reading.cutoff_verdict == Verdict::MixedSources {
      found.push("mixed sources by cutoff".to_string());
    }
    found.push(cutoff);
    found.extend(guess);
    found.push(
      if scanned {
        "no grid"
      } else {
        "grid not scanned"
      }
      .to_string(),
    );
    return found;
  }
  if reading.cutoff_hz.is_some() {
    found.push(cutoff);
  }
  found.extend(guess);
  found.extend(
    reading
      .flags
      .iter()
      .map(|flag| flag_words(*flag).to_string()),
  );
  if reading.verdict != Verdict::Inconclusive {
    found.push(format!("confidence {:.2}", reading.confidence));
  }
  found
}

/// the encoder family of a grid in a word or two; `None` for the family that names no encoder,
/// and for one the program has no word for
fn family_words(family: Family) -> Option<&'static str> {
  match family {
    Family::Lame => Some("LAME"),
    Family::Helix => Some("Helix"),
    Family::Apple => Some("Apple"),
    Family::Faac => Some("FAAC"),
    Family::Nero => Some("Nero"),
    Family::FfmpegOrFdk => Some("ffmpeg or fdk-aac"),
    Family::Ffmpeg => Some("ffmpeg"),
    Family::FdkAac => Some("fdk-aac"),
    Family::Fraunhofer => Some("Fraunhofer"),
    Family::Shine => Some("Shine"),
    Family::Libopus => Some("libopus"),
    // Untagged, and what the library's enum, which is non-exhaustive, may hold besides
    _ => None,
  }
}

pub fn flag_words(flag: Flag) -> &'static str {
  match flag {
    Flag::Upsampled => "upsampled",
    Flag::BitPadded => "bit padded",
    Flag::DualMono => "dual mono",
  }
}
