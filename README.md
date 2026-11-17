# earwitness

[![crates.io](https://img.shields.io/crates/v/earwitness)](https://crates.io/crates/earwitness)
[![docs.rs](https://img.shields.io/docsrs/earwitness)](https://docs.rs/earwitness)
[![ci](https://github.com/tetra-fox/earwitness/actions/workflows/ci.yml/badge.svg)](https://github.com/tetra-fox/earwitness/actions/workflows/ci.yml)
[![license](https://img.shields.io/crates/l/earwitness)](#license)

Reads an audio file's spectrum a second at a time and judges whether the audio is what its
format claims, such as a FLAC made from an MP3. It comes as a program, `earwitness`, and as a
library.

## What it reads

Each one-second window of the file is measured: its loudness, the frequency its content stops at
(the cutoff), how sharply it stops there, gaps in the band below the cutoff and steps inside a
band that reaches the top. A lossy encoder throws away the top of the band with a steep
low-pass at a frequency that depends on its bitrate, and decoding the result to FLAC or WAV
does not bring it back. The reading looks at the spread over all windows, not an average, and
gives one of four verdicts:

- **consistent**: nearly every window reaches the top of the band the format claims, or a lossy
  file stops where its own bitrate cuts
- **likely transcode**: nearly every window stops at the same known encoder cutoff with a sharp
  shelf, with a guess of the encoder setting (`mp3 ~128 kbps`, ...)
- **mixed sources**: part of the time or part of the band is band-limited while the rest is full
  band, as when a lossy intro or stem is mixed into lossless audio
- **inconclusive**: too little audio, measures that fit no pattern, or a cut the analyzer
  cannot tell apart from an honest one

Beside the verdict it flags audio converted up from a lower sample rate (`upsampled`) and
samples that claim more than 16 bits but carry no more (`bit_padded`), and it reports a summary
in words, what made the reading less reliable, and measures such as the effective bit depth,
the noise floor, stereo correlation, DC offset and clipping.

It decodes FLAC, ALAC, WAV, AIFF, CAF, MP3, AAC and Vorbis, in their own files and in MP4/M4A,
Ogg and Matroska/WebM, through [symphonia](https://github.com/pdeljanov/Symphonia). Opus is not
decoded: the library reads such a file as inconclusive, and the program reports it as not read
as audio.

## What it cannot tell

A reading is evidence, not proof. The analyzer looks at where and how the content stops, so:

- A lossless recording that was filtered with a sharp low-pass where an encoder would cut (some
  masters are cut at 16 or 19 kHz) reads the same as a transcode. Nothing in the spectrum tells
  them apart.
- The highest bitrates (MP3 at 320 kbps, AAC) cut near 20 kHz, where many recordings are filtered
  when they are made. A sharp cut there reads as inconclusive, never as a transcode, so those
  transcodes are not caught.
- The rules prefer saying they cannot tell over calling an honest file fake, so many transcodes
  read as inconclusive rather than as likely transcodes. An inconclusive reading is not a clean
  one.
- Content that is band-limited by nature (old or lo-fi recordings, quiet or dark material) can
  stop low without any encoder. A gentle roll-off reads as inconclusive, but a recording that
  happens to stop sharply near an encoder cutoff can be misread.
- Fewer than about 30 seconds of audio above silence is too little for a reading.
- Only the first two channels of a surround file are read.

## The program

```sh
cargo install earwitness
```

```console
$ earwitness check *.flac
honest.flac: consistent, confidence 0.73
fake-128.flac: likely transcode, cutoff 16.8 kHz, mp3 ~128 kbps, confidence 0.98
fake-320.flac: inconclusive, cutoff 20.2 kHz
mixed.flac: mixed sources, cutoff 16.8 kHz, mp3 ~128 kbps, confidence 0.75
```

With `--json` each file is one JSON object on a line of its own: the format the file claims,
its length, how many windows were read, and the full reading under each profile.

The exit status is 0 when every file is consistent or inconclusive, 1 when at least one reads as
a likely transcode or mixed sources, and 2 when a file could not be read or an argument was
wrong, so a script can act on it. A file the analyzer cannot open as audio prints `not read as
audio` with the reason, and counts as one that could not be read:

```sh
for f in *.flac; do earwitness check "$f" > /dev/null || mv "$f" suspect/; done
```

Every file is read in full unless `--max-full-minutes MINUTES --sampled-windows COUNT` say how
long a file read in full may be and how many one-second windows a longer one is read at.

### Thresholds

Every threshold is data. `earwitness check --defaults` prints the defaults as JSON:

```json
{ "measure": { "silenceDbfs": -70.0, "cutoffRangeDb": 60.0, ... },
  "reading": { "sharpShelfDb": 30.0, "transcodeShare": 0.85, ..., "profiles": [...] } }
```

`--profile FILE` reads with the thresholds in FILE in place of the defaults. Keys left out keep
their default and unknown keys are refused. The option can be repeated to read each file under
several profiles, each named after its file; profiles with the same `measure` thresholds share
one decode of the file.

## The library

```toml
[dependencies]
earwitness = { version = "0.1", default-features = false }
```

The default `cli` feature builds the program and its dependencies (clap, serde_json); a library
user turns it off.

```rust,no_run
use std::path::Path;
use std::time::{Duration, Instant};

use earwitness::engine::{self, Canceled, Settings};
use earwitness::{MeasureThresholds, ReadingThresholds, Verdict, reading};

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

`engine::run` blocks the thread it runs on. It reports progress through `progress`, stops and
returns `Canceled` once `canceled` says so, and stops with what it has at `deadline`. Measuring
is the slow step and reading is quick, so a sweep over reading thresholds measures each file
once. The `picture` module holds the spectrogram and waveform data each window carries.

The `testing` feature adds `fixtures`: the generated audio (shaped noise from a fixed seed,
written as FLAC and through the LAME MP3 encoder) the crate's own tests read. It builds LAME
from source, so it needs a C compiler and `make`.

The library logs through `tracing` and installs no subscriber.

## License

[AGPL-3.0-only](LICENSE).
