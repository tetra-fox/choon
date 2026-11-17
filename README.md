# choon

_Checks if a choon is proper, mate._

[![crates.io](https://img.shields.io/crates/v/choon)](https://crates.io/crates/choon)
[![docs.rs](https://img.shields.io/docsrs/choon)](https://docs.rs/choon)
[![ci](https://github.com/tetra-fox/choon/actions/workflows/ci.yml/badge.svg)](https://github.com/tetra-fox/choon/actions/workflows/ci.yml)
[![license](https://img.shields.io/crates/l/choon)](#license)

Tells whether an audio file is what its format claims, such as a FLAC made from an MP3. A
command-line tool and a Rust library.

## Features

- Spots lossless files decoded from lossy ones by the encoder's cutoff in the spectrum
- Guesses the encoder setting behind a transcode (`mp3 ~128 kbps`, ...)
- Flags upsampled audio and bit-padded samples
- Finds lossy parts mixed into lossless audio (an intro, a stem)
- Exit codes and JSON output for scripts
- Every threshold is data, readable from a profile file

## Methodology

Each one-second window of the file is measured: its loudness, where its content stops (the
cutoff), how sharply it stops, gaps below the cutoff and steps inside a full band. A lossy encoder
cuts the top of the band with a steep low-pass at a frequency set by its bitrate, and decoding to
FLAC or WAV does not bring it back. The verdict comes from how the cutoffs spread across all
windows.

| Verdict          | Means                                                                                                     |
| ---------------- | --------------------------------------------------------------------------------------------------------- |
| consistent       | nearly every window reaches the top of the claimed band, or a lossy file stops where its own bitrate cuts |
| likely transcode | nearly every window stops at one known encoder cutoff with a sharp shelf                                  |
| mixed sources    | part of the time or part of the band is band-limited, the rest full band                                  |
| inconclusive     | too little audio, no pattern, or a cut indistinguishable from an honest one                               |

Beside the verdict: the `upsampled` and `bit_padded` flags, a summary in words, what made the
reading less reliable, and measures such as effective bit depth, noise floor, stereo
correlation, DC offset and clipping.

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
honest.flac: consistent, confidence 0.73
fake-128.flac: likely transcode, cutoff 16.8 kHz, mp3 ~128 kbps, confidence 0.98
fake-320.flac: inconclusive, cutoff 20.2 kHz
mixed.flac: mixed sources, cutoff 16.8 kHz, mp3 ~128 kbps, confidence 0.75
```

| Option                                     | Does                                                                                   |
| ------------------------------------------ | -------------------------------------------------------------------------------------- |
| `--json`                                   | one JSON object per file and line: claimed format, length, windows read, every reading |
| `--profile FILE`                           | read with the thresholds in FILE instead of the defaults; repeat for several profiles  |
| `--defaults`                               | print the default thresholds as JSON                                                   |
| `--max-full-minutes M --sampled-windows N` | read files longer than M minutes at N one-second windows instead of in full            |

A profile is `{"measure": {...}, "reading": {...}}`. Keys left out keep their default, unknown
keys are refused, and profiles with the same `measure` thresholds share one decode.

| Exit status | When                                           |
| ----------- | ---------------------------------------------- |
| 0           | every file consistent or inconclusive          |
| 1           | at least one likely transcode or mixed sources |
| 2           | a file not read as audio, or a wrong argument  |

```sh
for f in *.flac; do choon check "$f" > /dev/null || mv "$f" suspect/; done
```

### Library

```toml
[dependencies]
choon = { version = "0.1", default-features = false }
```

```rust,no_run
use std::path::Path;
use std::time::{Duration, Instant};

use choon::engine::{self, Canceled, Settings};
use choon::{MeasureThresholds, ReadingThresholds, Verdict, reading};

fn main() -> Result<(), Canceled> {
  let settings = Settings {
    max_full_seconds: 900.0,
    sampled_windows: 240,
    deadline: Instant::now() + Duration::from_secs(120),
    progress: None,
    canceled: None,
    thresholds: MeasureThresholds::default(),
  };
  let measured = engine::run(Path::new("track.flac"), &settings)?;
  let reading = reading::read(&measured, &ReadingThresholds::default());
  match reading.verdict {
    Verdict::LikelyTranscode | Verdict::MixedSources => println!("lossy: {}", reading.summary),
    Verdict::Consistent | Verdict::Inconclusive => println!("{:?}", reading.verdict),
  }
  Ok(())
}
```

- `engine::run` blocks its thread; `progress`, `canceled` and `deadline` report, stop and bound it
- Measuring is slow and reading is quick: measure once, read under many thresholds
- `picture` holds each window's spectrogram and waveform data
- Logs through `tracing`, installs no subscriber

| Feature         | Adds                                                                                          |
| --------------- | --------------------------------------------------------------------------------------------- |
| `cli` (default) | the `choon` program and its dependencies (clap, serde_json)                                   |
| `testing`       | `fixtures`, the generated audio the tests read; builds LAME, so needs a C compiler and `make` |

## Limitations

- A lossless master filtered sharply where an encoder cuts (16 or 19 kHz) reads like a transcode
- MP3 320 and AAC cut near 20 kHz, where many recordings are filtered anyway: read as
  inconclusive, so those transcodes are missed
- Many transcodes read as inconclusive, because the rules call a file a transcode only on a clear pattern
- Naturally band-limited audio (old, lo-fi, dark) can stop near an encoder cutoff by chance
- Needs about 30 seconds of non-silent audio
- Reads only the first two channels of a surround file

## References

The frame-grid detection under development builds on this work on the compression history of
decoded audio:

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

The filterbanks follow the codec standards: ISO/IEC 11172-3 (MPEG-1 Audio Layer III) and
ISO/IEC 13818-7 (MPEG-2 AAC).

## License

[AGPL-3.0-only](LICENSE)
