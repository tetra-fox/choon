//! thresholds the analyzer measures and reads with, all data the caller passes

use std::fmt;

use crate::grid::Family;

/// a cutoff at or above this fraction of the band's top counts as reaching it, when a window is
/// measured and when it is read
const FULL_BAND: f64 = 0.9;

/// Thresholds each window is measured with.
///
/// Changing one means measuring the audio again.
///
/// No value is checked against a range. One outside what its field describes, or not a number,
/// gives measures that mean nothing, and no panic.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
  feature = "serde",
  serde(default, deny_unknown_fields, rename_all = "camelCase")
)]
pub struct MeasureThresholds {
  /// Level in dBFS under which a window is near-silent.
  ///
  /// A near-silent window has no cutoff.
  pub silence_dbfs: f64,
  /// Range in dB under the loudest band that the cutoff lies in.
  ///
  /// The cutoff is the highest band within this range.
  pub cutoff_range_db: f64,
  /// Margin in dB of the cutoff line over quantization noise.
  ///
  /// The line never sits closer to the noise of the samples' bit depth, so a quiet passage's
  /// quantization noise does not read as content.
  pub floor_margin_db: f64,
  /// Gap in Hz to the bands compared across a frequency.
  pub shelf_gap_hz: f64,
  /// Width in Hz of the bands compared across a frequency.
  pub shelf_span_hz: f64,
  /// Fraction of the top a cutoff needs for an inner step search.
  ///
  /// A window is searched for a step inside its band when its cutoff is at or above this
  /// fraction of the band's top.
  pub full_band: f64,
  /// Lowest frequency in Hz an inner step is looked for at.
  pub inner_shelf_from_hz: f64,
  /// Start in Hz of the band gaps are counted in.
  ///
  /// It is where MP3's top scale-factor band sits.
  pub holes_from_hz: f64,
  /// Top in Hz of the band gaps are counted in.
  pub holes_to_hz: f64,
  /// Depth in dB under its neighbors at which a bin is a gap.
  pub hole_depth_db: f64,
  /// Whether MP3 and AAC are scanned for at the other rate too.
  ///
  /// It applies to a file at 44.1 or 48 kHz. The scan at the other of the two rates finds a
  /// decode that was converted between them ([`OtherRate`](crate::grid::OtherRate)). It takes
  /// about as long as the scan for MP3 and AAC at the file's own rate.
  pub scan_other_rate: bool,
  /// Whether WMA is scanned for.
  ///
  /// The WMA scan takes about a quarter as long as all the others together. With it off, a
  /// WMA decode can still read as possibly lossy by its cutoff.
  pub scan_wma: bool,
}

impl Default for MeasureThresholds {
  fn default() -> Self {
    Self {
      silence_dbfs: -70.0,
      cutoff_range_db: 60.0,
      floor_margin_db: 10.0,
      shelf_gap_hz: 100.0,
      shelf_span_hz: 500.0,
      full_band: FULL_BAND,
      inner_shelf_from_hz: 10_000.0,
      holes_from_hz: 16_000.0,
      holes_to_hz: 20_000.0,
      hole_depth_db: 20.0,
      scan_other_rate: true,
      scan_wma: true,
    }
  }
}

/// Thresholds the measures are read with.
///
/// A reading is quick, so these can be swept over audio measured once.
///
/// No value is checked against a range. One outside what its field describes, or not a number,
/// gives a reading that means nothing, and no panic.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
  feature = "serde",
  serde(default, deny_unknown_fields, rename_all = "camelCase")
)]
pub struct ReadingThresholds {
  /// Least drop in dB across the cutoff for an encoder's low-pass.
  ///
  /// A smaller drop is a natural roll-off.
  pub sharp_shelf_db: f64,
  /// Fraction of the top from which a cutoff counts as full band.
  ///
  /// A cutoff at or above this fraction of the band's top counts as reaching it. Set below
  /// [`MeasureThresholds::full_band`], it counts as full some windows that measuring never
  /// searched for inner steps.
  pub full_band: f64,
  /// Fraction of the top under which a gentle roll-off is not judged.
  ///
  /// Content that rolls off gently below it is band-limited by nature, and the reading says it
  /// cannot tell.
  pub natural_limit: f64,
  /// Least inner step in dB that reads as a band-limited layer.
  ///
  /// A step of at least this size inside a full band is a band-limited layer under it.
  pub inner_shelf_db: f64,
  /// Least step in dB at a top-bitrate cutoff that casts doubt.
  ///
  /// Such a step under a band that reaches the top reads as something the analyzer cannot tell,
  /// which costs an honest file nothing. Honest spectra step by a decibel or two there.
  pub high_step_db: f64,
  /// Height in dB over the cutoff line that makes a band content.
  ///
  /// The band above a step is content only at this height or more. Nearer the line it is the
  /// recording's noise floor, hiss or dither.
  pub clear_db: f64,
  /// Fewest non-silent windows a reading needs.
  pub min_windows: usize,
  /// Fewest seconds of non-silent audio a reading needs.
  pub min_seconds: f64,
  /// Seconds past `min_seconds` with little to go on.
  ///
  /// Within this many seconds over `min_seconds` a reading says it has little to go on. There
  /// each second moves the shares its rules compare by about 2.5 percent, so a few odd
  /// seconds can tip it.
  pub few_seconds_over: f64,
  /// Share of windows at the top of the band for consistent content.
  ///
  /// Content reads as reaching the top of its band where at least this share of windows does.
  /// Where this share of windows reaches the top or stops without a sharp shelf, the content
  /// reads as a gentle roll-off: consistent from [`natural_limit`](Self::natural_limit) of the
  /// top up, inconclusive below it. A lossy file where neither holds is read by its
  /// [`ceiling`](Self::ceiling).
  pub consistent_share: f64,
  /// Share of windows that stop at one encoder cutoff for a transcode.
  pub transcode_share: f64,
  /// Distance in Hz at which a cutoff still shares the common one.
  pub cluster_hz: f64,
  /// Percentile of a lossy file's cutoffs read as its encoder's ceiling.
  pub ceiling: f64,
  /// Share of windows each of two sources has for mixed sources.
  pub mixed_share: f64,
  /// Band tops in Hz of the sample rates audio is commonly made at.
  ///
  /// Content that stops with a cliff at one, in a file whose band reaches past it, reads as
  /// made at that rate and is flagged ([`Flag::Upsampled`](crate::Flag::Upsampled)). Where a
  /// profile's cutoff is within `cluster_hz` of the edge no flag is raised, as the cliff fits a
  /// transcode from that encoder as well.
  pub lower_rate_edges: Vec<f64>,
  /// Band edges of rates some old equipment recorded at.
  ///
  /// They are left out of `lower_rate_edges` because they sit on encoder cutoffs (18.9 kHz
  /// beside 192 kbps MP3's). A cliff at one reads as that encoder, with a limitation naming the
  /// rate.
  pub other_rate_edges: Vec<RateEdge>,
  /// Correlation above which two channels hold the same audio.
  ///
  /// It applies together with a side channel at or below `mono_side_db`.
  pub mono_correlation: f64,
  /// Side-to-mid level in dB at or below which channels match.
  ///
  /// At or below this level of the side channel relative to the mid channel, the two channels
  /// hold the same audio.
  pub mono_side_db: f64,
  /// Highest confidence a reading has, from 0 to 1.
  ///
  /// A reading by a grid has this confidence itself, so a cap that is negative or not a number
  /// is the confidence of such a reading.
  pub confidence_cap: f64,
  /// Median window level in dBFS under which a file is quiet.
  ///
  /// A quiet file makes a reading less sure.
  pub quiet_dbfs: f64,
  /// Bitrate in kbit/s from which a cutoff may be a filter.
  ///
  /// Profiles at this bitrate and above cut where some recordings are filtered when they are
  /// made, so a sharp shelf there reads as something the analyzer cannot tell, never as a
  /// transcode.
  pub high_bitrate_kbps: u32,
  /// Cutoffs of common lossy encoder settings, lowest first.
  pub profiles: Vec<EncoderProfile>,
  /// Cutoffs of the settings of encoder families a grid names.
  ///
  /// They name the setting beside a grid's family ([`Found::setting`](crate::grid::Found::setting)),
  /// and no verdict reads them. `profiles` holds LAME's cutoffs, so LAME is listed here only
  /// where a cutoff names more settings than its profile says.
  pub family_profiles: Vec<FamilyProfile>,
  /// Robust z over which the AAC scan finds a frame grid.
  ///
  /// It is compared with [`CodecScan::z`](crate::grid::CodecScan::z). A lower value finds more
  /// grids in decodes that hide them, such as clipped ones, and finds them in more lossless
  /// files whose counts happen to dip at one offset.
  pub aac_grid_z: f64,
  /// Fewest segments that put their lowest count at the file's offset, for an AAC grid.
  ///
  /// It is compared with [`CodecScan::segments_agreeing`]. A decode carries one grid from start
  /// to end, so every segment finds it at the same offset. The segments of a lossless file can
  /// agree on an offset too, so agreement alone is no grid: it only narrows what the z
  /// threshold finds. A file that holds a lossy section only in part falls short, and so does
  /// a Vorbis decode, whose grid the AAC scan finds at an offset that moves between segments.
  ///
  /// [`CodecScan::segments_agreeing`]: crate::grid::CodecScan::segments_agreeing
  pub aac_grid_segments_agreeing: usize,
  /// Robust z over which the MP3 scan finds a frame grid.
  ///
  /// It is compared with [`CodecScan::z`](crate::grid::CodecScan::z), as `aac_grid_z` is.
  pub mp3_grid_z: f64,
  /// Fewest segments that put their lowest count at the file's offset, for an MP3 grid.
  ///
  /// It is compared with [`CodecScan::segments_agreeing`], as `aac_grid_segments_agreeing` is.
  ///
  /// [`CodecScan::segments_agreeing`]: crate::grid::CodecScan::segments_agreeing
  pub mp3_grid_segments_agreeing: usize,
  /// Fewest segments that agree, for a grid in part of the file at an uncut decode's offset.
  ///
  /// An encoder's delay puts the grid of its decode at a fixed offset when the start was not
  /// cut ([`Family`](crate::grid::Family)), a libvorbis stream's long blocks sit at one place
  /// within each 128 samples, and an AC-3 stream's first block starts at the first sample.
  /// Fewer segments than the codec's own threshold asks for (`aac_grid_segments_agreeing` and
  /// those of MP3, Vorbis and AC-3) show a grid in part of the file there, with the same z, as
  /// the segments of a lossless file rarely agree on that one offset.
  pub uncut_grid_segments_agreeing: usize,
  /// Robust z over which the MP2 scan finds a frame grid.
  ///
  /// It is compared with [`CodecScan::z`](crate::grid::CodecScan::z) of
  /// [`Scan::mp2`](crate::grid::Scan::mp2).
  pub mp2_grid_z: f64,
  /// Robust z over which an MP2 count every segment agrees on finds a frame grid.
  ///
  /// The MP2 scan counts at 32 phases, so three segments share one by chance far more often
  /// than three share an offset of another codec's grid, and four far less often than three.
  pub mp2_all_agreeing_grid_z: f64,
  /// Robust z over which an MP2 count at an uncut decode's phase finds a frame grid.
  ///
  /// An MP2 decode whose start was not cut has its count at one phase of the 32, which the
  /// segments of a lossless file rarely agree on. An MP3 decode's count can sit there too, so
  /// it applies where MP3's quantizer steps show in no segment.
  pub mp2_uncut_grid_z: f64,
  /// Fewest segments that put their lowest count at the file's phase, for an MP2 grid.
  ///
  /// It is compared with [`CodecScan::segments_agreeing`], as `mp3_grid_segments_agreeing` is.
  ///
  /// [`CodecScan::segments_agreeing`]: crate::grid::CodecScan::segments_agreeing
  pub mp2_grid_segments_agreeing: usize,
  /// Robust z over which the Vorbis scan finds a frame grid.
  ///
  /// It is compared with [`CodecScan::z`](crate::grid::CodecScan::z) of
  /// [`Scan::vorbis`](crate::grid::Scan::vorbis).
  pub vorbis_grid_z: f64,
  /// Fewest segments whose lowest count sits a whole number of steps from the file's offset.
  ///
  /// A Vorbis stream's short blocks move its long ones by 128 samples each, so its segments
  /// rarely share an offset and always share one modulo 128. It is compared with the
  /// segments of [`CodecScan::segment_offset`](crate::grid::CodecScan::segment_offset) that
  /// do.
  pub vorbis_grid_segments_agreeing: usize,
  /// Robust z over which the WMA scan finds a frame grid.
  ///
  /// It is compared with [`CodecScan::z`](crate::grid::CodecScan::z) of
  /// [`Scan::wma`](crate::grid::Scan::wma).
  pub wma_grid_z: f64,
  /// Fewest segments that put their lowest count at the file's offset, for a WMA grid.
  ///
  /// It is compared with [`CodecScan::segments_agreeing`], as `mp3_grid_segments_agreeing` is.
  ///
  /// [`CodecScan::segments_agreeing`]: crate::grid::CodecScan::segments_agreeing
  pub wma_grid_segments_agreeing: usize,
  /// Robust z over which the AC-3 scan finds a frame grid.
  ///
  /// It is compared with [`CodecScan::z`](crate::grid::CodecScan::z) of
  /// [`Scan::ac3`](crate::grid::Scan::ac3).
  pub ac3_grid_z: f64,
  /// Fewest segments that put their lowest count at the file's offset, for an AC-3 grid.
  ///
  /// It is compared with [`CodecScan::segments_agreeing`], as `mp3_grid_segments_agreeing` is.
  ///
  /// [`CodecScan::segments_agreeing`]: crate::grid::CodecScan::segments_agreeing
  pub ac3_grid_segments_agreeing: usize,
  /// Robust z a segment's own count passes to show a grid in part of the file.
  ///
  /// It is compared with each of [`CodecScan::segment_z`](crate::grid::CodecScan::segment_z)
  /// whose segment puts its lowest count at the file's offset, where no grid shows in the
  /// file as a whole.
  pub part_grid_z: f64,
  /// Fewest segments that pass `part_grid_z`, for a grid in part of the file, one at least.
  pub part_grid_segments: usize,
  /// Robust z one segment's own count passes to show a grid in part of the file by itself.
  ///
  /// It applies where the file's offset is one an uncut decode has, as
  /// `uncut_grid_segments_agreeing` does. One segment of a lossless file can stand far out at
  /// some offset, and rarely at that one.
  ///
  /// The scan at the other rate takes one such segment too, for a decode converted afterwards.
  pub lone_grid_z: f64,
  /// Fewest segments whose subband count is lowest at a block-aligned phase, for a grid.
  ///
  /// It is compared with the segments of [`Scan::mp2`](crate::grid::Scan::mp2) whose phase is
  /// one an uncut MP3 or Vorbis decode has. Their blocks start a whole number of the
  /// filterbank's 32 samples apart, so every segment's count is lowest at the same phase,
  /// however little it stands out. The reading names Vorbis where a segment sits at the
  /// phase only its decodes have, and MP3 otherwise.
  ///
  /// It is compared too with the segments at the phase an uncut MP2 decode has, which read
  /// as MP2 where MP3's quantizer steps show in none.
  pub block_phase_segments: usize,
  /// Robust z over which the AAC scan at the other rate finds a frame grid.
  ///
  /// It is compared with the z of [`OtherRate::aac`](crate::grid::OtherRate::aac) and of
  /// [`OtherRate::aac_sine`](crate::grid::OtherRate::aac_sine), with the agreement
  /// `aac_grid_segments_agreeing` asks for. Decodes of other codecs dip further at the other
  /// rate than lossless files do at their own, so it stands over `aac_grid_z`.
  pub other_rate_aac_z: f64,
  /// Fewest scalefactor bands on whole steps in a segment, for an MP3 grid.
  ///
  /// It is compared with each of
  /// [`Mp3Steps::segment_whole`](crate::grid::Mp3Steps::segment_whole), the bands on whole
  /// steps of MP3's quantizer, where no count stands out enough for a grid: as many segments
  /// as `mp3_grid_segments_agreeing` must reach it, one at least. A band of lossless audio
  /// lands on a whole step by chance a few times in a file.
  pub mp3_whole_steps: usize,
  /// Fewest segments that reach `mp3_whole_steps`, for an MP3 grid in part of the file.
  ///
  /// One at least. It applies where fewer segments reach it than a grid in the whole file
  /// asks for. One segment is enough where its lowest count is at the file's offset and that
  /// offset is one an uncut decode has.
  pub mp3_whole_steps_segments: usize,
  /// Fewest scalefactor bands on whole steps in a segment, for an AAC grid.
  ///
  /// It is compared with each of
  /// [`AacSteps::segment_whole`](crate::grid::AacSteps::segment_whole), the bands on whole
  /// steps of AAC's quantizer, under each window. The steps show a grid no count stands out
  /// enough for, and name the window of one a count found.
  pub aac_whole_steps: usize,
  /// Fewest segments that reach `aac_whole_steps`, for an AAC grid, one at least.
  pub aac_whole_steps_segments: usize,
  /// Fewest frames on Opus's lattice at one offset, for an Opus grid.
  ///
  /// It is compared with [`OpusScan::frames`](crate::grid::OpusScan::frames), the count over
  /// the four segments. A frame of lossless audio lands on the lattice by chance at some
  /// offset in some files, never at the same offset often.
  pub opus_grid_frames: usize,
  /// Fewest frames on Opus's lattice in the windows, for an Opus grid.
  ///
  /// It is compared with [`OpusScan::uncut_frames`](crate::grid::OpusScan::uncut_frames), the
  /// count on the one grid a decode has when nothing was cut from its start.
  pub opus_uncut_frames: usize,
  /// Fewest frames on Opus's lattice at another level, at one offset.
  ///
  /// It is compared with [`OpusShifted::frames`](crate::grid::OpusShifted::frames), the count
  /// over the four segments, where neither count at the decoder's own level makes an Opus
  /// grid. The search for the level gives chance more frames than `opus_grid_frames` meets.
  pub opus_shifted_frames: usize,
  /// Fewest frames on Opus's lattice at another level in the windows.
  ///
  /// It is compared with
  /// [`OpusShifted::uncut_frames`](crate::grid::OpusShifted::uncut_frames), as
  /// `opus_shifted_frames` is with its count.
  pub opus_shifted_uncut_frames: usize,
  /// Fewest frames on Opus's lattice once the pitch post-filter is taken off, in the windows.
  ///
  /// It is compared with [`OpusUnfiltered::frames`](crate::grid::OpusUnfiltered::frames),
  /// where no count at the decoder's level, at another level or of frames under 20 ms makes
  /// an Opus grid. The search for the filter gives chance more frames than `opus_uncut_frames`
  /// meets.
  pub opus_unfiltered_frames: usize,
  /// Fewest frames under 20 ms on Opus's lattice at one offset.
  ///
  /// It is compared with [`OpusShort::frames`](crate::grid::OpusShort::frames) of each
  /// length, the count over the four segments, where no count of 20 ms frames makes an Opus
  /// grid.
  pub opus_short_frames: usize,
  /// Fewest frames under 20 ms on Opus's lattice in the windows.
  ///
  /// It is compared with [`OpusShort::uncut_frames`](crate::grid::OpusShort::uncut_frames)
  /// of each length, as `opus_short_frames` is with its count.
  pub opus_short_uncut_frames: usize,
  /// Rules that name an MP3 encoder family by its quantizer steps.
  ///
  /// They are held against [`Mp3Steps::neighbours`](crate::grid::Mp3Steps::neighbours), and
  /// the family they name stands before the one the grid's offset fits.
  pub mp3_family_steps: FamilySteps,
  /// Rules that name an AAC encoder family by its quantizer steps.
  ///
  /// They are held against [`AacSteps::neighbours`](crate::grid::AacSteps::neighbours) under
  /// the Kaiser-Bessel-derived window, where the grid's offset and window name no one encoder.
  pub aac_family_steps: FamilySteps,
  /// Fewest different pairs of steps among Opus's hybrid frames.
  ///
  /// It is compared with [`OpusHybrid::steps`](crate::grid::OpusHybrid::steps) and with
  /// [`OpusHybrid::uncut_steps`](crate::grid::OpusHybrid::uncut_steps), where no other count
  /// makes an Opus grid. The frames counted must meet `opus_hybrid_share` too.
  pub opus_hybrid_steps: usize,
  /// Fewest hybrid frames as a share of the frames tested, from 0 to 1.
  ///
  /// The two bands a hybrid frame is tested on land on the lattice by chance in a few frames
  /// of a hundred, which in a long file is more pairs of steps than `opus_hybrid_steps`.
  pub opus_hybrid_share: f64,
}

/// Share of the bands or pairs of bands that a file's quantizer steps are counted in.
///
/// [`Mp3Neighbours`](crate::grid::Mp3Neighbours) and
/// [`AacNeighbours`](crate::grid::AacNeighbours) hold the counts, and say which bands a
/// region is: three regions for MP3, four for AAC, counted from 0. A share of no bands, of a
/// region the codec does not have, or that a codec's counts do not hold meets no bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "camelCase"))]
#[non_exhaustive]
pub enum StepShare {
  /// Pairs whose two steps differ, of all pairs, from 0 to 1.
  Moved,
  /// Pairs of a region with one step, of the region's pairs, from 0 to 1.
  SameIn(usize),
  /// Pairs of a region whose higher band is finer, of the region's pairs, from 0 to 1.
  FinerIn(usize),
  /// Pairs of a region one quarter power apart, of its pairs that differ, from 0 to 1.
  ///
  /// AAC alone: an MP3 encoder moves a step by two quarter powers at the least.
  OneIn(usize),
  /// Pairs far apart, of the pairs that differ, from 0 to 1.
  ///
  /// Far is six quarter powers or more for MP3, four or more for AAC.
  Far,
  /// Pairs of a region far apart, of the region's pairs that differ, from 0 to 1.
  FarIn(usize),
  /// Pairs whose higher band is two quarter powers finer, of all pairs, from 0 to 1.
  ///
  /// MP3 alone.
  TwoFiner,
  /// Pairs whose higher band is two quarter powers coarser, of all pairs, from 0 to 1.
  ///
  /// MP3 alone.
  TwoCoarser,
  /// Bands at the coarsest step of their channel, of the bands ranked, from 0 to 1.
  Coarsest,
  /// The same share of a region's bands, from 0 to 1.
  ///
  /// AAC alone.
  CoarsestIn(usize),
  /// Bands whose largest quantized value is one, of a region's bands, from 0 to 1.
  ///
  /// For MP3 the regions are bands 0 to 5, 6 to 11, and 12 up.
  OnesIn(usize),
  /// Granules read as mid and side, of all granules, from 0 to 1.
  ///
  /// MP3 alone.
  Joint,
  /// Mean of log2 of one more than the largest quantized value, over MP3's bands 0 to 5.
  LowLog,
  /// The same mean over MP3's bands 12 up.
  HighLog,
}

/// Range a share of a file's quantizer steps lies in for an encoder family.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct StepBound {
  /// Share the range is of.
  pub share: StepShare,
  /// Lowest value of the share, where it has one.
  #[cfg_attr(feature = "serde", serde(default))]
  pub min: Option<f64>,
  /// Highest value of the share, where it has one.
  #[cfg_attr(feature = "serde", serde(default))]
  pub max: Option<f64>,
}

impl StepBound {
  fn at_least(share: StepShare, min: f64) -> Self {
    Self {
      share,
      min: Some(min),
      max: None,
    }
  }

  fn at_most(share: StepShare, max: f64) -> Self {
    Self {
      share,
      min: None,
      max: Some(max),
    }
  }
}

/// Ranges that the quantizer steps of one encoder family's decodes lie in.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct StepRule {
  /// Family the ranges name.
  pub family: Family,
  /// Ranges a decode's shares all lie in.
  pub bounds: Vec<StepBound>,
}

/// Rules that name an encoder family by how it moves the quantizer's step between bands.
///
/// An encoder sets each scalefactor band's step, and encoders differ in how often, how far
/// and which way they move it from one band to the next. The shares of a decode's bands and
/// pairs of bands ([`StepShare`]) are held against each rule, and the decode is named where
/// the rules of one family alone hold. The steps are read on the grid wherever it sits, so a
/// rule names a decode whose start was cut too.
///
/// The rules the analyzer ships with were fitted on LAME, Helix, Fraunhofer's codec, Shine,
/// BladeEnc and GOGO for MP3, and on ffmpeg's encoder and fdk-aac for AAC. Another
/// encoder, or another version of one, can fall inside a rule or outside its own.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(
  feature = "serde",
  serde(deny_unknown_fields, rename_all = "camelCase")
)]
pub struct FamilySteps {
  /// Fewest pairs of neighbouring bands, over all regions.
  pub min_pairs: usize,
  /// Fewest pairs in each region.
  ///
  /// AAC's top region, which a low cutoff empties, is not asked for any.
  pub min_region_pairs: usize,
  /// Rules, each naming one family; several may name the same one.
  pub rules: Vec<StepRule>,
}

impl FamilySteps {
  /// the rules for MP3 the analyzer ships with
  fn mp3() -> Self {
    use StepShare::{
      Coarsest, Far, FarIn, FinerIn, HighLog, Joint, Moved, OnesIn, SameIn, TwoCoarser,
    };
    let (least, most) = (StepBound::at_least, StepBound::at_most);
    // the bounds were placed on decodes of 200 recordings, whole and with their starts cut:
    // 100 through LAME 3.90.3 to 3.100, Helix, Shine, BladeEnc and GOGO at 96 to 320 kbit/s,
    // 60 of them through Fraunhofer's codec as Windows ships it and 40 through Apple's, and
    // the corpus's LAME decodes of 100 more. each stands 0.02 clear of every decode of
    // another encoder that meets its rule's other bounds, Shine's by 0.001 and 0.1, and
    // LAME's on the far pairs of its middle region and on its granules as mid and side by
    // 0.05: GOGO, made from LAME's source, and LAME 3.90.3 and 3.93.1 at a constant bitrate
    // are the neighbours they are drawn against.
    //
    // tested on 26 recordings no bound was placed on, through the same encoders: of the
    // decodes with their starts cut the rules name 228 of 910 of LAME, 137 of 156 of Helix,
    // 145 of 156 of Fraunhofer and 70 of 104 of Shine, none of 260 of BladeEnc and GOGO, and
    // no decode as another encoder's
    Self {
      min_pairs: 2000,
      min_region_pairs: 200,
      rules: vec![
        StepRule {
          family: Family::Lame,
          bounds: vec![
            least(FarIn(1), 0.234),
            least(FarIn(2), 0.031),
            least(Joint, 0.05),
            least(Moved, 0.289),
            most(OnesIn(1), 0.32),
          ],
        },
        StepRule {
          family: Family::Helix,
          bounds: vec![
            least(TwoCoarser, 0.157),
            most(Far, 0.119),
            most(SameIn(2), 0.675),
          ],
        },
        // the bounds on the low region's finer pairs and on the middle one's same pairs keep
        // out Apple's encoder, whose decodes move few steps
        StepRule {
          family: Family::Fraunhofer,
          bounds: vec![
            most(Coarsest, 0.247),
            least(SameIn(1), 0.462),
            most(SameIn(1), 0.8),
            least(SameIn(2), 0.413),
            most(TwoCoarser, 0.136),
            least(FinerIn(0), 0.17),
          ],
        },
        // every encoder leaves the steps alone at the highest bitrates, where its values are
        // large, and Shine alone at the lower ones
        StepRule {
          family: Family::Shine,
          bounds: vec![most(Moved, 0.0014), most(HighLog, 2.39)],
        },
      ],
    }
  }

  /// the rules for AAC the analyzer ships with
  fn aac() -> Self {
    use StepShare::{Coarsest, FinerIn, Moved, OneIn, SameIn};
    let (least, most) = (StepBound::at_least, StepBound::at_most);
    // placed on the decodes of 100 of those recordings through fdk-aac and Nero, 60 through
    // ffmpeg's encoder with the corpus's ffmpeg decodes of 100 more, at 96 to 256 kbit/s,
    // each bound 0.02 clear as MP3's are.
    //
    // tested on the same 26 recordings: of the decodes with their starts cut the rules name
    // 112 of 130 of ffmpeg and 141 of 208 of fdk-aac, and no decode as another encoder's.
    // Nero's steps lie beside fdk-aac's at 256 kbit/s: bounds placed for Nero name 6 of the
    // test's fdk-aac decodes as Nero's, so Nero has no rule and is named by its offset alone
    Self {
      min_pairs: 2000,
      min_region_pairs: 200,
      rules: vec![
        StepRule {
          family: Family::Ffmpeg,
          bounds: vec![most(SameIn(0), 0.383), most(SameIn(3), 0.297)],
        },
        StepRule {
          family: Family::FdkAac,
          bounds: vec![
            most(Moved, 0.479),
            least(OneIn(0), 0.385),
            most(FinerIn(0), 0.292),
            least(Coarsest, 0.055),
          ],
        },
      ],
    }
  }
}

/// Band top of a rate some old equipment recorded at.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct RateEdge {
  /// Half the rate, in Hz.
  pub hz: f64,
  /// Rate and who used it, as a limitation names it.
  pub rate: String,
}

/// Lossy encoder setting known by where it cuts off.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct EncoderProfile {
  /// Cutoff at 44.1 and 48 kHz, in Hz.
  pub hz: f64,
  /// Codec that cuts here, spelled as a grid's codec is written.
  pub codec: String,
  /// Setting of that codec that cuts here, in words.
  pub setting: String,
  /// Another codec that cuts here too.
  #[cfg_attr(feature = "serde", serde(default))]
  pub also: Option<String>,
  /// Highest nominal bitrate cut here, in kbit/s.
  pub kbps: u32,
}

impl EncoderProfile {
  fn mp3(hz: f64, setting: &str, kbps: u32) -> Self {
    Self {
      hz,
      codec: "MP3".to_string(),
      setting: setting.to_string(),
      also: None,
      kbps,
    }
  }
}

/// Setting of one encoder family known by where it cuts off.
#[derive(Clone, Debug, PartialEq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(deny_unknown_fields))]
pub struct FamilyProfile {
  /// Family whose encoder cuts here.
  pub family: Family,
  /// Cutoff at 44.1 and 48 kHz, in Hz.
  pub hz: f64,
  /// Setting that cuts here, in words.
  pub setting: String,
}

/// Writes the profile's name: the codec, its setting, then the other codec after "or".
impl fmt::Display for EncoderProfile {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    write!(f, "{} {}", self.codec, self.setting)?;
    match &self.also {
      Some(also) => write!(f, " or {also}"),
      None => Ok(()),
    }
  }
}

impl Default for ReadingThresholds {
  fn default() -> Self {
    Self {
      sharp_shelf_db: 30.0,
      full_band: FULL_BAND,
      natural_limit: 0.75,
      inner_shelf_db: 20.0,
      high_step_db: 10.0,
      clear_db: 10.0,
      min_windows: 20,
      min_seconds: 30.0,
      few_seconds_over: 10.0,
      consistent_share: 0.9,
      transcode_share: 0.85,
      cluster_hz: 500.0,
      ceiling: 0.9,
      mixed_share: 0.15,
      // 22.05, 32, 44.1 and 48 kHz
      lower_rate_edges: vec![11_025.0, 16_000.0, 22_050.0, 24_000.0],
      other_rate_edges: [
        (18_900.0, "37.8 kHz (PlayStation and CD-i audio)"),
        (15_000.0, "30 kHz (some older samplers)"),
        (13_020.0, "26.04 kHz (some older samplers)"),
      ]
      .map(|(hz, rate)| RateEdge {
        hz,
        rate: rate.to_string(),
      })
      .to_vec(),
      mono_correlation: 0.9999,
      mono_side_db: -90.0,
      confidence_cap: 0.98,
      quiet_dbfs: -40.0,
      high_bitrate_kbps: 320,
      // LAME's defaults are the commonest; other MP3 encoders and AAC cut a little lower at the
      // same bitrate
      profiles: vec![
        EncoderProfile::mp3(11_000.0, "~64 kbps", 64),
        EncoderProfile::mp3(13_500.0, "~80 kbps", 80),
        EncoderProfile::mp3(15_100.0, "~96 kbps", 96),
        EncoderProfile::mp3(16_000.0, "~128 kbps", 128),
        EncoderProfile::mp3(17_000.0, "~128 kbps", 128),
        EncoderProfile::mp3(17_500.0, "~160 kbps", 160),
        EncoderProfile::mp3(18_600.0, "~192 kbps", 192),
        EncoderProfile::mp3(19_400.0, "~224 kbps", 224),
        // LAME 3.100 cuts 224 kbps here too, so the name says both
        EncoderProfile::mp3(19_530.0, "~224-256 kbps", 256),
        EncoderProfile {
          also: Some("AAC".to_string()),
          ..EncoderProfile::mp3(20_000.0, "~320 kbps", 320)
        },
        EncoderProfile::mp3(20_500.0, "~320 kbps", 320),
      ],
      // the median cutoffs of two-minute excerpts of 60 recordings through each encoder.
      // LAME 3.90.3 and 3.93.1 cut 128 kbit/s where later versions cut 96, and fdk-aac's
      // third VBR mode cuts where its 128 and 160 kbit/s do
      family_profiles: [
        (Family::Lame, 15_100.0, "~96-128 kbps"),
        (Family::Ffmpeg, 16_000.0, "~96 kbps"),
        (Family::Ffmpeg, 17_250.0, "~128 kbps"),
        (Family::Ffmpeg, 17_960.0, "~160 kbps"),
        (Family::Ffmpeg, 19_270.0, "~192 kbps"),
        (Family::FdkAac, 14_310.0, "~96 kbps"),
        (Family::FdkAac, 16_000.0, "~128-160 kbps"),
        (Family::FdkAac, 16_530.0, "VBR 4"),
        (Family::FdkAac, 17_020.0, "~192-256 kbps"),
        (Family::FdkAac, 19_190.0, "VBR 5"),
      ]
      .map(|(family, hz, setting)| FamilyProfile {
        family,
        hz,
        setting: setting.to_string(),
      })
      .to_vec(),
      // of 409 lossless files of the tuning, AAC reaches z 5.2 and its decodes start at 14.9
      // with three segments agreeing. an MP3 decode's AAC count passes that in one segment, and
      // a lossless file's MP3 count with an MP3 section in two
      aac_grid_z: 6.0,
      aac_grid_segments_agreeing: 3,
      mp3_grid_z: 7.0,
      mp3_grid_segments_agreeing: 3,
      // of 5,400 lossless files of the corpus none has two segments agreeing at such an offset
      // with a z over its codec's
      uncut_grid_segments_agreeing: 2,
      // the lossless files reach 5.0 with two segments on the step at most
      vorbis_grid_z: 6.0,
      vorbis_grid_segments_agreeing: 3,
      // the lossless files reach 8.8 in one segment, and WMA decodes start at 208
      wma_grid_z: 12.0,
      wma_grid_segments_agreeing: 3,
      // a segment of the held-out lossless files reaches 6.6, and lower thresholds find few
      // more decodes
      ac3_grid_z: 8.0,
      ac3_grid_segments_agreeing: 3,
      // of 1,026 lossless files none has three segments agreeing, and of 332 MP2 decodes
      // 327 have four, from z 4.1
      mp2_grid_z: 12.0,
      mp2_all_agreeing_grid_z: 4.0,
      // of 5,400 lossless files of the corpus none has three segments at that phase
      mp2_uncut_grid_z: 6.0,
      mp2_grid_segments_agreeing: 3,
      // a lossless file's segment reaches 20 alone, and 7.1 beside another at its offset
      part_grid_z: 7.0,
      part_grid_segments: 2,
      // a segment of the corpus's lossless files reaches 8.3 at such an offset
      lone_grid_z: 12.0,
      // four segments sit at two phases of 32 by chance once in 65,536 files, and none of the
      // corpus's 5,400 lossless files does
      block_phase_segments: 4,
      // the tuning's files with no AAC behind a conversion reach 9.63 there, and its converted
      // AAC decodes that show a grid start at 16.7
      other_rate_aac_z: 13.0,
      // the second stretch of the corpus's files that were never MP3 reaches eight whole
      // steps, of 4,490 files
      mp3_whole_steps: 15,
      mp3_whole_steps_segments: 2,
      // a stretch of the tuning's Vorbis decodes reaches 138 whole steps under one window, and of
      // its files that were never AAC 11
      aac_whole_steps: 200,
      aac_whole_steps_segments: 1,
      // the tuning's files that were never Opus reach one frame in the stretches and one in
      // the windows
      opus_grid_frames: 3,
      opus_uncut_frames: 3,
      // with a level searched for they reach four frames in the stretches and one in the
      // windows, apart from one master that carries such frames on an uncut decode's grid
      opus_shifted_frames: 6,
      opus_shifted_uncut_frames: 4,
      // of frames under 20 ms they reach one in the stretches and one in the windows
      opus_short_frames: 3,
      opus_short_uncut_frames: 3,
      // of 3,837 lossless files of the corpus none has such a frame, of 40 pairs searched
      opus_unfiltered_frames: 4,
      mp3_family_steps: FamilySteps::mp3(),
      aac_family_steps: FamilySteps::aac(),
      // the pilot files that were never Opus reach nine pairs of steps at this share, and a
      // share of 0.04 with this many pairs
      opus_hybrid_steps: 30,
      opus_hybrid_share: 0.1,
    }
  }
}

impl ReadingThresholds {
  /// fewer seconds of non-silent audio than this give a reading little to go on, which it says
  pub(crate) fn few_seconds(&self) -> f64 {
    self.min_seconds + self.few_seconds_over
  }

  /// the profile whose cutoff is nearest `hz`, when one is within `cluster_hz`
  pub(crate) fn profile_at(&self, hz: f64) -> Option<&EncoderProfile> {
    self
      .profiles
      .iter()
      .filter(|p| (p.hz - hz).abs() <= self.cluster_hz)
      .min_by(|a, b| (a.hz - hz).abs().total_cmp(&(b.hz - hz).abs()))
  }
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn thresholds_read_from_json_keep_the_defaults_they_do_not_name() {
    let measure: MeasureThresholds = serde_json::from_str(r#"{"cutoffRangeDb": 50.0}"#).unwrap();
    assert_eq!(
      measure,
      MeasureThresholds {
        cutoff_range_db: 50.0,
        ..MeasureThresholds::default()
      }
    );
    let reading: ReadingThresholds = serde_json::from_str(r#"{"sharpShelfDb": 40.0}"#).unwrap();
    assert_eq!(
      reading,
      ReadingThresholds {
        sharp_shelf_db: 40.0,
        ..ReadingThresholds::default()
      }
    );
  }

  #[test]
  fn the_family_rules_are_read_back_from_the_json_they_are_written_as() {
    let defaults = ReadingThresholds::default();
    let json = serde_json::to_string(&defaults).unwrap();
    assert!(
      json.contains(r#"{"family":"shine","bounds":[{"share":"moved","min":null,"max":0.0014}"#),
      "{json}"
    );
    assert!(json.contains(r#"{"share":{"sameIn":3},"min":null,"max":0.297}"#));
    assert!(json.contains(r#"{"family":"fdk_aac","hz":16000.0,"setting":"~128-160 kbps"}"#));
    let read: ReadingThresholds = serde_json::from_str(&json).unwrap();
    assert_eq!(read, defaults);
    // a bound names its share and either end
    let rules = r#"{"mp3FamilySteps": {"minPairs": 1, "minRegionPairs": 1, "rules": [
      {"family": "helix", "bounds": [{"share": {"farIn": 2}, "max": 0.1}]}]}}"#;
    let read: ReadingThresholds = serde_json::from_str(rules).unwrap();
    let bound = &read.mp3_family_steps.rules[0].bounds[0];
    assert_eq!(
      (bound.share, bound.min, bound.max),
      (StepShare::FarIn(2), None, Some(0.1))
    );
    refused::<ReadingThresholds>(r#"{"mp3FamilySteps": {"minPairs": 1}}"#);
    refused::<ReadingThresholds>(r#"{"familyProfiles": [{"family": "lame", "hz": 1.0}]}"#);
  }

  fn refused<T: serde::de::DeserializeOwned>(json: &str) {
    assert!(
      serde_json::from_str::<T>(json).is_err(),
      "{json} is refused"
    );
  }

  #[test]
  fn a_key_no_threshold_has_is_refused_at_every_level() {
    refused::<MeasureThresholds>(r#"{"cutoffRange": 50.0}"#);
    refused::<ReadingThresholds>(r#"{"sharpShelf": 40.0}"#);
    refused::<ReadingThresholds>(
      r#"{"profiles": [{"hz": 16000.0, "codec": "a", "setting": "b", "kbps": 128, "guid": "A"}]}"#,
    );
    refused::<ReadingThresholds>(
      r#"{"otherRateEdges": [{"hz": 15000.0, "rate": "30 kHz", "source": "a"}]}"#,
    );
  }
}
