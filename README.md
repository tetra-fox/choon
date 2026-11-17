# choon

_Proper lossless, innit?_

[![crates.io](https://img.shields.io/crates/v/choon)](https://crates.io/crates/choon)
[![docs.rs](https://img.shields.io/docsrs/choon)](https://docs.rs/choon)
[![ci](https://github.com/tetra-fox/choon/actions/workflows/ci.yml/badge.svg)](https://github.com/tetra-fox/choon/actions/workflows/ci.yml)
[![license](https://img.shields.io/crates/l/choon)](#license)

Detect lossless audio files made from lossy ones, such as a FLAC transcoded from an MP3. A
command-line tool and a Rust library.

## Features

- Spots lossless files decoded from MP3 or AAC by the frame grid the encoder leaves, tuned on
  LAME at 96 to 320 kbps and V0 to V9 and on ffmpeg's AAC at 96 to 256 kbps
- Reads the encoder's cutoff in the spectrum, which alone raises a doubt
- Guesses the encoder setting behind a transcode, such as `mp3 ~128 kbps`
- Flags upsampled audio, bit-padded samples and two channels holding the same audio
- Finds lossy parts mixed into lossless audio (an intro, a stem)
- Exit codes and JSON output for scripts
- Every threshold is data, readable from a profile file

## Methodology

Each one-second window of the file is measured:

- its loudness
- where its content stops (the cutoff) and how sharply
- gaps below the cutoff
- steps inside a full band

A lossy encoder cuts the top of the band with a steep low-pass at a frequency set by its bitrate,
and decoding to FLAC or WAV does not bring it back. The cutoff reading comes from how the cutoffs
spread across all windows.

In a file that claims a lossless format, four stretches of the samples are also scanned for the
frame grid of an MP3 or AAC encoder. The encoder transforms its input in frames on a fixed grid
and zeroes many transform values. Running its transform again on the decoded samples, with frames
where the encoder put them, finds those values back at the level of the samples' rounding noise.

The scan counts the values above that noise at every offset of each codec's grid. A grid is one
offset that counts far fewer than the rest; an MP3 grid also needs at least three stretches
agreeing on it. It does not depend on where the encoder cut, so MP3 at 320 kbps or V0 and AAC at
256 kbps show one too.

| Verdict          | Description                                                                 |
| ---------------- | --------------------------------------------------------------------------- |
| consistent       | nearly every window reaches the top of the claimed band                     |
| likely transcode | the samples carry an MP3 or AAC frame grid                                  |
| mixed sources    | part of the time or of the band is band-limited, the rest full band         |
| possibly lossy   | the cutoff reads as a transcode or mixed sources, and no frame grid shows   |
| inconclusive     | too little audio, no pattern, or a cut indistinguishable from an honest one |

- A lossy file is also consistent when it stops where its own bitrate cuts
- Without a grid scan, as in a lossy file, a likely transcode is nearly every window stopping at
  one known encoder cutoff with a sharp shelf
- Mixed sources is read only without a grid scan

A reading also holds:

- the `upsampled`, `bit_padded` and `dual_mono` flags
- a summary in words
- what made it less reliable
- measures such as effective bit depth, noise floor, stereo correlation, DC offset and clipping

## Supported formats

| Codecs                                       | Containers                                   |
| -------------------------------------------- | -------------------------------------------- |
| FLAC, ALAC, WAV, AIFF, CAF, MP3, AAC, Vorbis | their own files, MP4/M4A, Ogg, Matroska/WebM |

Decoding is done by [symphonia](https://github.com/pdeljanov/Symphonia). Opus is not decoded.

## Installation

```sh
cargo install choon
```

## Usage

### CLI

```console
$ choon check *.flac
honest.flac: consistent, confidence 0.80
fake-128.flac: likely transcode: MP3 grid at offset 48 (z 226), cut at 16.7 kHz, where mp3 ~128 kbps cuts
fake-320.flac: likely transcode: MP3 grid at offset 48 (z 131), cut at 20.2 kHz, where mp3 ~320 kbps or aac cuts
cut-19.flac: possibly lossy, cannot confirm: no codec grid; the spectrum reads as an encoder's cutoff (cut at 19.3 kHz, like mp3 ~224 kbps)
```

| Option                                     | Description                                               |
| ------------------------------------------ | --------------------------------------------------------- |
| `--json`                                   | print each file as one line of JSON                       |
| `--profile FILE`                           | read with the thresholds in FILE in place of the defaults |
| `--defaults`                               | print the default thresholds as JSON                      |
| `--max-full-minutes M --sampled-windows N` | read files longer than M minutes at N one-second windows  |

- A JSON line holds the claimed format, the length, the windows read, the grid scan and every
  reading
- A profile is `{"measure": {...}, "reading": {...}}`; keys left out keep their default and
  unknown keys are refused
- Repeating `--profile` reads each file with several profiles; those with the same `measure`
  thresholds share one decode

| Exit code | Description                                           |
| --------- | ----------------------------------------------------- |
| 0         | every file consistent, possibly lossy or inconclusive |
| 1         | at least one likely transcode or mixed sources        |
| 2         | a file that could not be read, or a wrong argument    |

```sh
for f in *.flac; do choon check "$f" > /dev/null || mv "$f" suspect/; done
```

### Library

```toml
[dependencies]
choon = { version = "0.1", default-features = false }
```

```rust,no_run
use choon::{ReadingThresholds, Settings, Verdict};

fn main() -> Result<(), choon::Error> {
  let measured = choon::measure("track.flac", &Settings::default())?;
  let reading = measured.read(&ReadingThresholds::default());
  match reading.verdict {
    Verdict::LikelyTranscode | Verdict::MixedSources => println!("lossy: {}", reading.summary),
    Verdict::Consistent | Verdict::PossiblyLossy | Verdict::Inconclusive => {
      println!("{:?}", reading.verdict)
    }
  }
  Ok(())
}
```

- `choon::measure` blocks its thread
- The grid scan runs on rayon's thread pool, the global one unless `choon::measure` is called
  inside another
- Measuring is slow and reading is quick: measure once, read under many thresholds
- `picture` holds each window's spectrogram and waveform data
- Logs through `tracing`

| Setting    | Description                                    |
| ---------- | ---------------------------------------------- |
| `coverage` | reads a long file at windows spread over it    |
| `deadline` | stops the analysis with what it has            |
| `canceled` | stops the analysis, which then reports nothing |
| `progress` | reports how far the analysis is                |

| Feature         | Description                                                                                  |
| --------------- | -------------------------------------------------------------------------------------------- |
| `cli` (default) | the `choon` program and its dependencies (clap, serde_json)                                  |
| `serde`         | `Serialize` and `Deserialize` on the thresholds and the measures, `Serialize` on the reading |
| `testing`       | `fixtures`, the generated audio the tests read                                               |

- `cli` turns `serde` on
- `testing` builds LAME, which needs a C compiler and `make`

## Limitations

- A lossless master filtered sharply where an encoder cuts (16 or 19 kHz) reads as possibly lossy
- Resampling after the decode removes the frame grid, so such a transcode reads as possibly lossy
  at most
- Opus leaves no grid the scan knows; a Vorbis decode's grid mostly reads as AAC's
- Clipping hides the grid frame by frame, so a decode made louder until it clips can lose it
- The grid thresholds were tuned on LAME's MP3 and ffmpeg's AAC only; other AAC encoders use
  their own windows
- A grid shows that the samples were decoded from MP3 or AAC at some point, as in a master built
  from such material
- A cutoff near 20 kHz, where MP3 320 and AAC cut and many recordings are filtered anyway, reads
  as inconclusive; only a grid calls such a file a transcode
- Naturally band-limited audio (old, lo-fi, dark) can stop near an encoder cutoff by chance
- Needs about 30 seconds of non-silent audio
- Reads only the first two channels of a surround file

## References

- O. Derrien, "Detection of genuine lossless audio files. Application to the MPEG-AAC codec",
  _Journal of the Audio Engineering Society_ 67(3), pp. 116-123, 2019.
  [doi:10.17743/jaes.2019.0002](https://doi.org/10.17743/jaes.2019.0002),
  [author's version](https://hal.science/hal-02055742).
- J. Lacroix, Y. Prime, A. Remy, O. Derrien, "Lossless Audio Checker: a software for the
  detection of upscaling, upsampling and transcoding in lossless musical tracks", AES 139th
  Convention, paper 9416, 2015. [AES E-Library](https://www.aes.org/e-lib/browse.cfm?elib=17972).
- R. Yang, Z. Qu, J. Huang, "Detecting digital audio forgeries by checking frame offsets",
  _Proceedings of the 10th ACM Workshop on Multimedia and Security_, pp. 21-26, 2008.
- R. Korycki, "Detection of montage in lossy compressed digital audio recordings", _Archives of
  Acoustics_ 39(1), pp. 65-72, 2014.
- T. Bianchi, A. De Rosa, M. Fontani, G. Rocciolo, A. Piva, "Detection and classification of
  double compressed MP3 audio tracks", _Proceedings of the 1st ACM Workshop on Information Hiding
  and Multimedia Security_, 2013.
  [Open access](https://iris.polito.it/bitstream/11583/2508475/2/bianchi_IHMMSEC13_OA.pdf).

The frame-grid scan's filterbanks follow the codec standards: ISO/IEC 11172-3 (MPEG-1 Audio
Layer III) and ISO/IEC 13818-7 (MPEG-2 AAC).

## License

[AGPL-3.0-only](LICENSE)
