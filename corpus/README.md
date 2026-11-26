# Corpus

Files whose history is known, made from lossless masters and read by `choon`: each master
encoded by LAME, ffmpeg, libopus and libvorbis, decoded, and written as FLAC. The table says how
many of the lossy files read as a transcode, and how many of the masters and their lossless
copies do.

```sh
just corpus prepare ~/music/lossless ~/music/live   # lists the masters, writes their encodes
just corpus run                                     # makes and reads every file
just corpus table                                   # counts the verdicts
```

`just corpus` runs `corpus/corpus` in a Nix shell that provides ffmpeg, LAME, SoX, mpg123,
FAAD2, Python and cargo. Without Nix, the program runs wherever those are on the path.

The masters are only ever read. No audio is kept in the repository.

## Commands

| Command         | Description                                                               |
| --------------- | ------------------------------------------------------------------------- |
| `prepare`       | lists the lossless files under each folder as masters, writes the encodes |
| `run`           | makes each file in memory, reads it, stores the reading and the measures  |
| `run --reread`  | reads the stored measures again under the checkout's rules                |
| `table`         | counts the stored readings by kind of file, a column for each folder      |

| Option          | Description                                                               |
| --------------- | ------------------------------------------------------------------------- |
| `--work DIR`    | where the list of masters, the encodes and the results are kept           |
| `--count N`     | take N masters, the same ones each time                                   |
| `--jobs N`      | files made and read at once                                               |
| `--labels FILE` | masters `table` counts apart                                              |
| `--misses`      | list each lossy file `table` counts as missed                             |

- A master is a FLAC, WAV, AIFF, WavPack, Monkey's Audio or ALAC file of one or two channels at
  44.1 kHz or more. `prepare` says which files it leaves out
- `--work` is `corpus/work` unless given. The encodes take about 200 MB a master
- `--count N` takes the first N masters of a fixed shuffle, so a larger count holds every
  smaller one
- `run` builds `corpus/reader` on the checkout first: a program on the library that prints a
  file's measures with its reading. A file measured by the same build is kept, so a stopped
  run resumes
- `run` ends by reading a twentieth of the files it read again, and exits 1 if a reading differs
- `run` writes `progress.log`, `status.json`, `errors.log` and `pids` under `DIR/run`

## Files

A master gives about 100 files:

| Kind                  | Files                                                     |
| --------------------- | --------------------------------------------------------- |
| Master and its copies | the master and its lossless copies                        |
| MP3 (LAME)            | CBR 96, 128, 160, 192, 256, 320 kbps and V0 to V9         |
| AAC (ffmpeg)          | 96, 128, 192, 256 kbps                                    |
| Opus (libopus)        | 64, 96, 128, 192, 256 kbps                                |
| Vorbis (libvorbis)    | q2 to q8 and q10                                          |
| MP2 (ffmpeg)          | 128, 192, 256, 384 kbps                                   |
| WMA (ffmpeg)          | 64, 128, 192, 320 kbps                                    |
| AC-3 (ffmpeg)         | 192, 384, 448, 640 kbps                                   |
| E-AC-3 (ffmpeg)       | 192, 384 kbps                                             |
| Encoded twice         | MP3, AAC, Opus or Vorbis decoded and encoded again as MP3 |
| Altered decodes       | decodes of five encodes, altered                          |

- The copies: resampled to the other rate and back, upsampled to 96 kHz, dithered to 16 bits,
  lowpassed at 16, 19 and 20 kHz
- The altered decodes are of MP3 CBR 128, CBR 320 and V0 and of AAC 128 and 256
- Each of those encodes is decoded to 16 bits, by ffmpeg and by mpg123, LAME or FAAD2, and
  that decode is dithered, gained by 3 dB up and down, peak normalized, and resampled between
  44.1 and 48 kHz

MP2, WMA, AC-3, E-AC-3 and Vorbis q10 are encoded in memory each run; the rest are kept under
`DIR/lossy`.

## Rereading

`run` keeps each file's measures as `VARIANT.measured.json.gz`. After a commit that changes
thresholds or rules and no measure, `run --reread` reads them again without the audio, which
takes seconds. It then measures 300 files anew and exits 1 if a measure differs from the stored
one: such a commit needs `run` in full.

## Labels

A master is lossless by its format only. One whose own file reads as a likely transcode or as
mixed sources is counted apart, as its copies are no clean files. `--labels FILE` names more
masters to count apart:

```json
{
  "lossy": { "0f3a9c2b1d": "made from an MP3" },
  "mixed": {},
  "doubt": {},
  "flagged": ["/bootlegs/"]
}
```

The keys of `lossy`, `mixed` and `doubt` are ids from `DIR/masters.json`, each with a note.
`flagged` holds parts of paths: a master whose path contains one is counted apart.
