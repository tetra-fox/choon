# choon

_Proper lossless, innit?_

[![crates.io](https://img.shields.io/crates/v/choon)](https://crates.io/crates/choon)
[![docs.rs](https://img.shields.io/docsrs/choon)](https://docs.rs/choon)
[![ci](https://github.com/tetra-fox/choon/actions/workflows/ci.yml/badge.svg)](https://github.com/tetra-fox/choon/actions/workflows/ci.yml)
[![license](https://img.shields.io/crates/l/choon)](#license)

Check that lossless audio is actually lossless. A command-line tool and a Rust library.

## Features

- Shows when a lossless file's samples were decoded from MP3, MP2, AAC, Vorbis, WMA, AC-3 or
  E-AC-3, by the frame grid the encoder leaves
- Shows when they were decoded from Opus, by the band energies its encoder quantizes
- Finds that grid after a conversion between 44.1 and 48 kHz
- Finds an Opus decode at 48 kHz or converted to 44.1 kHz, with or without a gain after the
  decode, in CELT and hybrid modes, in frames of 2.5 to 60 ms
- Tuned on LAME at 96 to 320 kbps and V0 to V9, ffmpeg's AAC at 96 to 256 kbps and libopus at
  16 to 256 kbps
- Names the [encoder family](docs/encoders.md#encoder-family) a grid fits, such as LAME, Helix,
  fdk-aac or libopus, by its quantizer's steps or its offset
- Guesses the encoder and setting behind a transcode, such as `MP3 (LAME) ~128 kbps`
- Reads the encoder's cutoff in the spectrum, which alone raises a doubt
- Flags upsampled audio, bit-padded samples and two channels holding the same audio
- Finds lossy parts mixed into lossless audio (an intro, a stem)
- Checks a whole library in one run: directories, a list of paths on stdin, files in parallel
- Exit codes and JSON output for scripts
- Every threshold is data, readable from a profile file

## Installation

Prebuilt binaries are available on the
[releases page](https://github.com/tetra-fox/choon/releases), or with cargo:

```sh
cargo install choon
```

## Usage

### CLI

```console
$ choon check music/
  Verdict           Cutoff    Encoder               File
  ----------------  --------  --------------------  -------------
music/
  possibly lossy    19.4 kHz  MP3 ~224 kbps         cut-19.flac
  likely transcode  16.8 kHz  MP3 (LAME) ~128 kbps  lame-128.flac
  likely transcode  20.2 kHz  MP3 (LAME) ~320 kbps  lame-320.flac

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
```

A terminal shows a progress bar per file and one for the run with the counts so far, then the
flagged files by directory and the counts. One file alone gets its reading in detail, with its
summary in sentences under `--verbose`.

Piped or with `--json`, each file is one line, in the order files finish:

```console
$ fd -e flac . music/ | choon check - | sort
music/cut-19.flac: possibly lossy (cutoff 19.4 kHz, MP3 ~224 kbps, no grid)
music/honest.flac: consistent (confidence 0.73)
music/lame-128.flac: likely transcode (MP3 grid, cutoff 16.8 kHz, MP3 (LAME) ~128 kbps)
music/lame-320.flac: likely transcode (MP3 grid, cutoff 20.2 kHz, MP3 (LAME) ~320 kbps)
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
| `-v`, `--verbose`                          | list every file in a terminal, flagged ones in detail     |
| `--color WHEN`                             | colour the verdicts: `auto`, `always` or `never`          |
| `--json`                                   | print each file as one line of JSON                       |
| `--profile FILE`                           | read with the thresholds in FILE in place of the defaults |
| `--defaults`                               | print the default thresholds as JSON                      |
| `--save-measures FILE`                     | write each file's measures to FILE, a line of JSON each   |
| `--from-measures FILE`                     | read the measures in FILE in place of audio files         |
| `--timeout SECONDS`                        | stop measuring a file after SECONDS                       |
| `--fast`                                   | skip the scans for WMA and for a converted sample rate    |
| `--max-full-minutes M --sampled-windows N` | read files longer than M minutes at N one-second windows  |

- A terminal's list names a directory given once, the directories under it from there, and
  each file by its name. A file given itself is listed under its directory, or by its path
  when no other file given shares the directory
- A directory whose files all read alike is one line, as
  `album/  12 of 12 likely transcode (MP3 grid, cutoff 16.5 to 16.9 kHz, MP3 (LAME) ~128 kbps)`
- The list fits the terminal's width: a name too long for its line has `...` in its middle, and
  a terminal under 60 columns gets the piped lines
- `--verbose` lists every file, with a column for each grid and its offset, and every
  directory file by file, then prints each flagged file in detail
- A walk skips hidden files and directories; `choon check --help` lists the extensions it reads
- A file named twice, or named and inside a directory named, is read once
- With no audio file to read, `choon: no audio files found` goes to stderr and the exit is 0,
  or 2 when a directory could not be read
- A FIFO is read as a file is once a writer opens it, and waits for one until then
- `--jobs` defaults to the number of cores and takes up to 1024; a file's grid scan runs on the
  same threads
- A line is complete when it is written, and lines come in the order files finish, so a
  consumer reads them while the run goes on and matches each to its input by `file`
- A JSON line holds the claimed format, the length, the windows read, the grid scan, an Opus
  file's modes and bandwidths, an HE-AAC file's core rate, a DSD file's one-bit rate, and every
  reading
- A path that is not Unicode has its bytes beside `file` on Unix, as `fileBytes`, a list of
  numbers
- On Unix a line holds a path's own bytes, as it is, so a script that matches files to
  readings reads the JSON
- A file the analyzer fails on is not read and says so, and the other files are still read
- A file out of time under `--timeout` prints a reading of the part measured, with a limitation
  that says so, and counts toward the exit code as any reading does. One with no window
  measured by then is not read
- A profile is `{"measure": {...}, "reading": {...}}`; keys left out keep their default and
  unknown keys are refused
- Repeating `--profile` reads each file with several profiles; those with the same `measure`
  thresholds share one decode
- A reading is named after its profile's file, so two profile files of one name are refused

| Exit code | Description                                           |
| --------- | ----------------------------------------------------- |
| 0         | every file consistent, possibly lossy or inconclusive |
| 1         | at least one likely transcode or mixed sources        |
| 2         | a file that could not be read, or a wrong argument    |
| 130       | an interrupt before every file was read               |

- On an interrupt (Ctrl-C, or SIGTERM or SIGHUP on Unix) no further file is started and the
  files under way are dropped. The files already read keep their lines, in the output and in
  the measures file, and stderr says `choon: interrupted: N of M files read`
- A file without a line was not read, so a script that goes on later gives those files again
- A second interrupt ends the program at once

```sh
for f in *.flac; do choon check "$f" > /dev/null; [ $? -eq 1 ] && mv "$f" flagged/; done
```

#### Measures file

Measuring decodes a file and reading does not. `--save-measures` keeps what a check measured,
and `--from-measures` reads it again under other reading thresholds:

```sh
choon check --save-measures measures.jsonl music/
for db in 20 25 30; do
  echo "{\"reading\": {\"sharpShelfDb\": $db}}" > shelf-$db.json
  choon check --from-measures measures.jsonl --profile shelf-$db.json
done
```

- A line holds one measuring of a file: `file`, with `fileBytes` as the JSON has it, the
  program's `version`, the `measure` thresholds and `measured`, the library's `Measured`
  without the spectrogram and waveform data
- A line is written when its file is read. A file not read has none, and a file read with
  profiles that measure differently has one for each measuring, one after the other
- A line takes about 300 bytes for each second of audio read: 68 kB for a 4-minute file
- `--from-measures` prints each file as the check printed it, lines, JSON or a terminal's
  tables, with the same exit code. No audio file is opened
- A reading's `measureSeconds` is 0 there, and `readSeconds` is the time the reading took
- Measures saved by version 0.2 hold none of the counts the encoder's steps are named by, so a
  grid read from them is named by its offset alone
- A profile reads a file only from a measuring made with its own `measure` thresholds. Without
  one the file is not read, and stderr or a terminal's `Not read` list names the profile and
  the file
- Measures saved with `--fast` are read again with `--fast`
- A line that does not parse is a file not read, as `not read (measures file line 12: ...)`,
  and is named `FILE:LINE` when it names no file

### Library

```toml
[dependencies]
choon = { version = "0.2", default-features = false }
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
- A FIFO waits for its writer when it is opened, no longer than the deadline or the cancel
  check lets the analysis run
- Builds for `wasm32-unknown-unknown` with default features off. A panic there is a trap, and
  no deadline can be set

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

| Decoder           | Panic                                          | Handling                           |
| ----------------- | ---------------------------------------------- | ---------------------------------- |
| ape-decoder 0.3.2 | on a damaged Monkey's Audio file               | logged at `warn`, frame skipped    |
| wavicle 0.1.0     | on a damaged WavPack block                     | block skipped                      |
| symphonia 0.6.1   | on an MP4 file with sample counts past 32 bits | the reading of the file ends there |

- Each can panic in a build with overflow checks, and the panic is caught
- Any other panic while a file is measured is the error `Error::Panicked`, and the panic's
  message goes where the panic hook writes it
- A build with `panic = "abort"` aborts

## Verdicts

| Verdict          | Description                                                                 |
| ---------------- | --------------------------------------------------------------------------- |
| consistent       | nearly every window reaches the top of the claimed band                     |
| likely transcode | the samples carry an MP3, MP2, AAC, Vorbis, WMA, AC-3 or Opus frame grid    |
| mixed sources    | a frame grid in part of the file, or part of the time or band band-limited  |
| possibly lossy   | the cutoff reads as a transcode or mixed sources, and no frame grid shows   |
| inconclusive     | too little audio, no pattern, or a cut indistinguishable from an honest one |

- A grid in two of the four stretches scanned reads as mixed sources, and so does one in a
  single stretch at the offset of a decode whose start was not cut
- The scan cannot tell how much of the file a lossy section covers
- A lossy file is also consistent when it stops where its own bitrate cuts
- No grid is looked for in a lossy file or a DSD file. A likely transcode there is nearly every
  window stopping at one known encoder cutoff with a sharp shelf
- Mixed sources by the cutoff is read only in such a file
- A file whose grid scan did not run, as FLAC in Matroska, is possibly lossy at most
- Beside the verdict, a reading holds the `upsampled`, `bit_padded` and `dual_mono` flags, a
  summary in words and what made it less reliable
- A reading holds measures such as effective bit depth, noise floor, stereo correlation, DC
  offset and clipping

## Supported formats

| Codecs                                        | Containers                                   |
| --------------------------------------------- | -------------------------------------------- |
| FLAC, ALAC, WAV, AIFF, CAF, MP3, Vorbis, Opus | their own files, MP4/M4A, Ogg, Matroska/WebM |
| AAC-LC, HE-AAC, HE-AAC v2                     | MP4/M4A, ADTS (`.aac`)                       |
| WavPack                                       | `.wv`                                        |
| Monkey's Audio                                | `.ape`                                       |
| DSD at 64 to 512 times 44.1 or 48 kHz         | DSF (`.dsf`), DSDIFF (`.dff`)                |

| Decoder                                                                    | Codecs                                  |
| -------------------------------------------------------------------------- | --------------------------------------- |
| [symphonia](https://github.com/pdeljanov/Symphonia)                        | FLAC, ALAC, WAV, AIFF, CAF, MP3, Vorbis |
| [symphonia-opus](https://github.com/jiztastamablastamarang/symphonia-opus) | Opus                                    |
| [rusty_aac](https://crates.io/crates/rusty_aac)                            | AAC                                     |
| [wavicle](https://crates.io/crates/wavicle)                                | WavPack                                 |
| [ape-decoder](https://crates.io/crates/ape-decoder)                        | Monkey's Audio                          |

[docs/formats.md](docs/formats.md) lists the files refused and the notes on each format.

## Methodology

The [`grid` module's documentation](https://docs.rs/choon/latest/choon/grid/) describes each
scan in full. [docs/encoders.md](docs/encoders.md) has the steps and offsets that name an
encoder family and the counts for each encoder tested.

### Spectrum

Each one-second window of the file is measured:

- its loudness
- where its content stops (the cutoff) and how sharply
- gaps below the cutoff
- steps inside a full band

A lossy encoder cuts the top of the band with a steep low-pass at a frequency set by its bitrate,
and decoding to FLAC or WAV does not bring it back. The cutoff reading comes from how the cutoffs
spread across all windows.

### Frame grid

An MP3, AAC, Vorbis, WMA or AC-3 encoder transforms its input in frames on a fixed grid and
zeroes many transform values. Running its transform again on the decoded samples, with frames
where the encoder put them, finds those values back at the level of the samples' rounding noise.

- Four stretches of a file that claims a lossless format are scanned
- A grid is one offset that counts far fewer values above that noise than the offsets near it,
  in at least three of the four stretches
- The count does not depend on where the encoder cut, so MP3 at 320 kbps or V0 and AAC at
  256 kbps show a grid too
- The values an MP3 or AAC encoder kept sit on its quantizer's steps, which show a grid where
  the count stands out too little
- A file at 44.1 or 48 kHz is scanned a second time with its stretches brought to the other rate

### Opus

Opus zeroes few values. Its CELT layer codes the energy of each of 21 bands in each 20 ms frame
as a step from a prediction, a whole number of 6 dB refined by a few bits. With Opus's transform
run again on the grid, each band's energy comes back and its step sits on that lattice.

- The scan counts the frames whose bands sit on the lattice, whatever the bitrate
- Hybrid-mode frames, frames under 20 ms and frames under the decoder's pitch filter each have
  a count of their own
- A gain after the decode moves the lattice. The scan takes the move off, which shows a decode
  and cannot tell what the gain was

## Results

choon at commit `ef4f5b5` read 84,321 FLAC files made from 840 lossless masters. Each lossy file
was encoded from a master, decoded, and written as FLAC.

- 238 masters are public: 94 recordings from the
  [Live Music Archive](https://archive.org/details/etree) and 144 mixtures of
  [MUSDB18-HQ](https://zenodo.org/records/3338373)
- 602 are from the author's library, whose audio cannot be shared

| Audio                                                         | Public         | Library          |
| ------------------------------------------------------------- | -------------- | ---------------- |
| MP3 (LAME), CBR 96 to 320 kbps and V0 to V9                   | 3,808 of 3,808 | 9,632 of 9,632   |
| AAC (ffmpeg), 96 to 256 kbps                                  | 952 of 952     | 2,408 of 2,408   |
| Opus (libopus), 64 to 256 kbps                                | 1,190 of 1,190 | 3,008 of 3,010   |
| Vorbis (libvorbis), q2 to q8                                  | 1,666 of 1,666 | 4,214 of 4,214   |
| Vorbis (libvorbis), q10                                       | 238 of 238     | 598 of 602       |
| MP2 (ffmpeg), 128 to 384 kbps                                 | 952 of 952     | 2,408 of 2,408   |
| WMA (ffmpeg), 64 to 320 kbps                                  | 952 of 952     | 2,408 of 2,408   |
| AC-3 (ffmpeg), 192 to 640 kbps                                | 942 of 952     | 2,386 of 2,408   |
| E-AC-3 (ffmpeg), 192 and 384 kbps                             | 475 of 476     | 1,201 of 1,204   |
| MP3, AAC, Opus or Vorbis encoded again as MP3                 | 1,428 of 1,428 | 3,612 of 3,612   |
| MP3 and AAC with gain, dither or peak normalization applied   | 4,760 of 4,760 | 12,039 of 12,040 |
| MP3 and AAC resampled between 44.1 and 48 kHz                 | 1,190 of 1,190 | 3,009 of 3,010   |
| MP3 and AAC decoded by ffmpeg to 16 bits, LAME, mpg123, FAAD2 | 3,808 of 3,808 | 9,632 of 9,632   |
| Lossless masters and their copies                             | 0 of 1,428     | 0 of 3,933       |

- Each cell is the files read as a likely transcode or as mixed sources. 40 of the lossy files
  read as mixed sources
- A master's copies are the master resampled to another rate and back, upsampled to 96 kHz,
  dithered to 16 bits, and lowpassed at 16, 19 and 20 kHz. 275 of them read as possibly lossy
  by their cutoff, all but 3 lowpassed at 16 or 19 kHz
- The 44 misses are decodes of 30 masters, 36 of them AC-3 or E-AC-3 at 384 kbps and up
- 301 more masters are left out, 11 of them public: the author knows them as made from lossy
  sources, or their own files carry a codec's frame grid
- The thresholds were tuned on these files, so the last row is no false-positive rate
- [docs/encoders.md](docs/encoders.md#tested-encoders) counts the decodes found of other
  encoders and versions

## Limitations

- A lossless master filtered sharply where an encoder cuts (16 or 19 kHz) reads as possibly lossy
- A cutoff near 20 kHz, where MP3 320 and AAC cut and many recordings are filtered anyway, reads
  as inconclusive; only a grid calls such a file a transcode
- Naturally band-limited audio (old, lo-fi, dark) can stop near an encoder cutoff by chance
- A grid shows that the samples were decoded from MP3, MP2, AAC, Vorbis, WMA, AC-3 or Opus at
  some point, as in a master built from such material
- Needs about 30 seconds of non-silent audio
- Reads only the first two channels of a surround file
- A damaged file is read as far as its decoder gets, and a reading does not always say that
  audio is missing

[docs/limitations.md](docs/limitations.md) lists what the grid and Opus scans miss.

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
