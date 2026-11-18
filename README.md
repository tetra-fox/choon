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
- Spots lossless files decoded from Opus by the band energies its encoder quantizes, at 48 kHz
  and after a conversion to 44.1 kHz, tuned on libopus at 16 to 256 kbps
- Reads Opus files, with the coding mode and audio bandwidth their packets state
- Reads WavPack, Monkey's Audio and DSD files, and HE-AAC with the band its decoder rebuilds
- Reads the encoder's cutoff in the spectrum, which alone raises a doubt
- Guesses the encoder setting behind a transcode, such as `mp3 ~128 kbps`
- Flags upsampled audio, bit-padded samples and two channels holding the same audio
- Finds lossy parts mixed into lossless audio (an intro, a stem)
- Checks a whole library in one run: directories, a list of paths on stdin, files in parallel
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

AAC gives a long block one of two windows, and the zeroed values come back only under the one
the encoder used. The scan runs under both: the Kaiser-Bessel-derived window of ffmpeg, fdk-aac
and Nero, and the sine window of Apple and FAAC.

The scan counts the values above that noise at every offset of each codec's grid. A grid is one
offset that counts far fewer than the rest; an MP3 grid also needs at least three stretches
agreeing on it. It does not depend on where the encoder cut, so MP3 at 320 kbps or V0 and AAC at
256 kbps show one too.

Opus zeroes few values. Its CELT layer codes the energy of each of 21 bands in each 20 ms frame
as a step from a prediction, a whole number of 6 dB refined by a few bits. With Opus's transform
run again on the grid, each band's energy comes back and its step sits on that lattice. The scan
counts the frames whose bands do, at every offset of the grid in the four stretches, and in up
to 60 of the windows on the one grid a decode has when nothing was cut from its start. Samples
at 44.1 kHz are brought back to the 48 kHz Opus runs at first. It does not depend on the
bitrate.

| Verdict          | Description                                                                 |
| ---------------- | --------------------------------------------------------------------------- |
| consistent       | nearly every window reaches the top of the claimed band                     |
| likely transcode | the samples carry an MP3, AAC or Opus frame grid                            |
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

| Codecs                                          | Containers                                   |
| ----------------------------------------------- | -------------------------------------------- |
| FLAC, ALAC, WAV, AIFF, CAF, MP3, Vorbis, Opus   | their own files, MP4/M4A, Ogg, Matroska/WebM |
| AAC-LC, HE-AAC, HE-AAC v2                       | MP4/M4A, ADTS (`.aac`)                       |
| WavPack                                         | `.wv`                                        |
| Monkey's Audio                                  | `.ape`                                       |
| DSD at 64 to 512 times 44.1 or 48 kHz           | DSF (`.dsf`), DSDIFF (`.dff`)                |

| Decoder                                                                          | Codecs         |
| -------------------------------------------------------------------------------- | -------------- |
| [symphonia](https://github.com/pdeljanov/Symphonia)                              | all but these  |
| [symphonia-opus](https://github.com/jiztastamablastamarang/symphonia-opus)       | Opus           |
| [rusty_aac](https://crates.io/crates/rusty_aac)                                  | AAC            |
| [wavicle](https://crates.io/crates/wavicle)                                      | WavPack        |
| [ape-decoder](https://crates.io/crates/ape-decoder)                              | Monkey's Audio |

- HE-AAC is decoded at its output rate, with spectral band replication and parametric stereo.
  The band above the core is the decoder's reconstruction, so its reading names no encoder
- WavPack is read as mono or stereo at 16, 24 or 32 bits or as float. A hybrid file is lossy
  without its correction file and is refused by name, as are 8-bit samples, more than two
  channels and streams older than WavPack 4
- Monkey's Audio is read at 8, 16 or 24 bits from version 3.99 on. Older files, float samples
  and 32-bit samples are refused by name
- DSD is measured as PCM at a sixteenth of its rate and at most 352.8 or 384 kHz: 176.4 kHz
  for DSD64. The filter passes 0.45 of that rate and removes what would fold onto it by 115 dB
- A DSD reading says where the band stops, which shows a source limited like 44.1 or 48 kHz
  PCM or cut by a lossy encoder. It cannot tell a DSD recording from PCM converted up, and the
  noise above the audio band reads as content
- DST-compressed DSDIFF is refused by name
- Opus is read in Ogg (`.opus`, `.ogg`), Matroska/WebM (`.mka`, `.webm`) and MP4
- An Opus file starts after its pre-skip and has its header's output gain applied
- Matroska does not say where the last Opus packet's audio ends, so up to 20 ms of padding is
  read with it

## Installation

```sh
cargo install choon
```

## Usage

### CLI

```console
$ choon check music/
music/fake-128.flac: likely transcode: MP3 grid at offset 1 (z 110), cut at 16.8 kHz, where mp3 ~128 kbps cuts
music/cut-19.flac: possibly lossy, cannot confirm: no codec grid; the spectrum reads as an encoder's cutoff (cut at 19.4 kHz, like mp3 ~224 kbps)
music/fake-320.flac: likely transcode: MP3 grid at offset 1 (z 126), cut at 20.2 kHz, where mp3 ~320 kbps or aac cuts

Verdict            Files
------------------ -------
consistent             1
likely transcode       2
mixed sources          0
possibly lossy         1
inconclusive           0

Files read             4
Files not read         0
Wall time          0.9 s
Files per minute     255

Verdict            Grid            Cutoff     Encoder guess          Path
------------------ --------------- ---------- ---------------------- ---------------------
possibly lossy                     19.4 kHz   mp3 ~224 kbps          music/cut-19.flac
likely transcode   MP3, offset 1   16.8 kHz   mp3 ~128 kbps          music/fake-128.flac
likely transcode   MP3, offset 1   20.2 kHz   mp3 ~320 kbps or aac   music/fake-320.flac
```

A terminal shows a progress bar per file and one for the run, each flagged file as it is found,
then these tables. One file alone gets its reading in detail. Piped or with `--json`, each file is
one line, in the order files finish:

```console
$ fd -e flac . music/ | choon check - | sort
music/cut-19.flac: possibly lossy, cannot confirm: no codec grid; the spectrum reads as an encoder's cutoff (cut at 19.4 kHz, like mp3 ~224 kbps)
music/fake-128.flac: likely transcode: MP3 grid at offset 1 (z 110), cut at 16.8 kHz, where mp3 ~128 kbps cuts
music/fake-320.flac: likely transcode: MP3 grid at offset 1 (z 126), cut at 20.2 kHz, where mp3 ~320 kbps or aac cuts
music/honest.flac: consistent, confidence 0.73
```

| Argument    | Description                                                         |
| ----------- | ------------------------------------------------------------------- |
| a file      | read whatever its name                                              |
| a directory | read with the directories under it, by audio extension              |
| `-`         | the paths listed on stdin, one per line, or NUL-separated with `-0` |

| Option                                     | Description                                               |
| ------------------------------------------ | --------------------------------------------------------- |
| `-j`, `--jobs N`                           | read up to N files at once, on N threads                  |
| `-L`, `--follow`                           | follow symbolic links in a directory                      |
| `-v`, `--verbose`                          | print every file in a terminal, flagged ones in detail    |
| `--color WHEN`                             | colour the verdicts: `auto`, `always` or `never`          |
| `--json`                                   | print each file as one line of JSON                       |
| `--profile FILE`                           | read with the thresholds in FILE in place of the defaults |
| `--defaults`                               | print the default thresholds as JSON                      |
| `--max-full-minutes M --sampled-windows N` | read files longer than M minutes at N one-second windows  |

- A walk skips hidden files and directories; `choon check --help` lists the extensions it reads
- `--jobs` defaults to the number of cores; a file's grid scan runs on the same threads
- A JSON line holds the claimed format, the length, the windows read, the grid scan, an Opus
  file's modes and bandwidths, an HE-AAC file's core rate, a DSD file's one-bit rate, and every
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
- ape-decoder 0.3.2 can panic on a damaged Monkey's Audio file in a build with overflow checks.
  The panic is caught, logged at `warn` and the frame skipped; a build with `panic = "abort"`
  aborts

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
- `testing` builds LAME and libopus, which need a C compiler, `make` and CMake

## Encoders

Decodes to FLAC of 40 recordings per setting, at each encoder's common bitrates:

| Codec  | Encoder                                           | Decodes found, of 40 |
| ------ | ------------------------------------------------- | -------------------- |
| MP3    | LAME 3.90.3, 3.93.1, 3.97, 3.98.4, 3.99.5, 3.100  | 38 to 40             |
| MP3    | Helix, Shine, BladeEnc, GOGO                      | 39 to 40             |
| AAC-LC | ffmpeg, fdk-aac                                   | 40                   |
| AAC-LC | Nero, Apple (CBR, constrained VBR, true VBR)      | 39 to 40             |
| AAC-LC | FAAC at 128 kbps and at quality 100               | 30 and 36            |
| AAC-LC | FAAC at 256 kbps                                  | 5                    |
| HE-AAC | Apple, fdk-aac, Nero at 48 and 64 kbps            | 4 to 21              |
| HE-AAC | Apple, fdk-aac, Nero, v2 at 32 kbps               | 0 to 3               |
| Vorbis | libvorbis 1.1.2 to 1.3.7, aoTuV, at q3 and q6     | 34 to 36             |
| Vorbis | libvorbis 1.2.0 and 1.3.7, aoTuV, at q9           | 18 to 23             |
| Opus   | libopus 1.1, 1.3.1, 1.5.2, 1.6.1 at 64 to 256     | 39                   |

- FAAC at 256 kbps codes nearly every frame as short blocks, which the scan does not analyse
- An HE-AAC decode's core was coded at half the sample rate under the decoder's filterbank
- A Vorbis decode is found by the AAC scan, whose windows are not Vorbis's
- The MP3 encoders of Apple and Fraunhofer were not tested

## Limitations

- A lossless master filtered sharply where an encoder cuts (16 or 19 kHz) reads as possibly lossy
- Resampling after the decode removes an MP3 or AAC frame grid, so such a transcode reads as
  possibly lossy at most
- A Vorbis decode's grid mostly reads as AAC's
- Opus is found in 20 ms frames (and the 40 and 60 ms packets made of them) coded as long
  blocks with the pitch post-filter off; a decode made mostly of other frames, one of 2.5, 5 or
  10 ms frames, and one in SILK or hybrid voice mode read as possibly lossy at most
- A gain change after an Opus decode moves the energies off the lattice
- Opus is looked for at 44.1 and 48 kHz only
- Clipping hides the grid frame by frame, so a decode made louder until it clips can lose it
- The grid thresholds were tuned on LAME's MP3 and ffmpeg's AAC; [Encoders](#encoders) lists
  what the scan finds of others
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

- J.-M. Valin, G. Maxwell, T. B. Terriberry, K. Vos, "High-quality, low-delay music coding in
  the Opus codec", AES 135th Convention, paper 8942, 2013.
  [arXiv:1602.04845](https://arxiv.org/abs/1602.04845).

The frame-grid scan's filterbanks follow the codec standards: ISO/IEC 11172-3 (MPEG-1 Audio
Layer III), ISO/IEC 13818-7 (MPEG-2 AAC) and RFC 6716 (Opus). Opus files are read as RFC 7845
(Ogg Encapsulation for the Opus Audio Codec) says.

## License

[AGPL-3.0-only](LICENSE)
