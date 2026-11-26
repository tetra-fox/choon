# Torture set

Files made to break `choon`: honest audio that looks like a codec's work, lossy decodes after
what people do to them, every container, and what disks and networks do to a file. All of it is
generated: noise and tones from ffmpeg, through each format's own program, then edited with
`dd`, `truncate` and `printf` at offsets the script computes. No recording is used, and the
files are not kept in the repository.

```sh
just torture        # writes about 2 GB here
just torture long   # adds files of 1, 6 and 24 hours and WAV files past 2 and 4 GB, about 1 GB
just torture-run    # reads every file with a release build and judges it
```

`just torture` runs `make.sh` in a Nix shell that provides ffmpeg, LAME, opus-tools,
vorbis-tools, fdkaac, flac, WavPack and Monkey's Audio. It also writes
`seeds/`, a second or less of each format, which `just fuzz-seed` copies into the fuzz corpus.

## Manifest

`make.sh` writes `manifest.tsv` beside the files, one line a case, tab-separated:

1. the file
2. what it should read as
3. where that expectation comes from, one or more of:
   - a section of the README, a file under `docs/`, `samples/README.md`, the program's help or
     a type's documentation
   - a rule in words where none of them states it
   - `nothing says` where nothing does
4. how the file was made

| Expectation        | Condition                                                                  |
| ------------------ | -------------------------------------------------------------------------- |
| `honest`           | the verdict is consistent or inconclusive, with no grid, and the exit is 0 |
| `consistent`       | the verdict is consistent                                                  |
| `lossy:CODEC`      | a grid of CODEC is found; `any` for whichever codec                        |
| `lossy:CODEC:NAME` | and the encoder family is NAME; `none` for no family after a cut start     |
| `atmost`           | it is read; `docs/limitations.md` says it may not be found                 |
| `as:FILE`          | the verdict, grid, family and cutoff are FILE's                            |
| `limited`          | it is not read, or its reading states what was not read                    |
| `refused:WORDS`    | it is not read, and the reason holds WORDS                                 |
| `unread`           | it is not read                                                             |
| `other`            | it is read, and no encoder family is named; the codec is not looked for    |
| `open`             | it is read or refused without a crash; nothing documents which             |

- A case that meets its condition is `as documented`, or `undocumented` for `other` and
  `open`, as nothing documents their reading

## File names

`<group>-<history>_<bits>-<kHz>.<ext>`, as in `samples/`, where the depth and rate matter.

- `A`: lossless from the start, and must never read as a likely transcode or mixed sources
- `B`: decodes of lossy files, and what was done to them after `-then-`
- `C`: containers, tags, muxers, damage and the edges of each format
- `D`: lengths

## Run

`run.py RESULTS.tsv BINARY...` reads each file alone with each binary as a line, as JSON and
as JSON sampled at 20 windows, with a minute's timeout (twenty for the hours-long files), and
writes the exit status, wall time and peak memory of every run. The manifest lists the long
files whether or not they were made, and a case whose file is not there is left out.
The first binary's reading is judged against the expectation. Another binary that reads the
file differently, and a sampled read that does, is noted beside it. A second binary built
with `-C debug-assertions -C overflow-checks` turns an integer overflow that a release build
wraps into a panic.

| Outcome            | Description                                                            |
| ------------------ | ---------------------------------------------------------------------- |
| `as documented`    | the case meets its condition                                           |
| `undocumented`     | an `open` case read or refused, or an `other` case read with no family |
| `worse`            | the reading or exit status is not the one expected                     |
| `FALSE ACCUSATION` | an honest file read as lossy or with a grid                            |
| `possibly lossy`   | an honest file read as possibly lossy                                  |
| `miss`             | no grid found in a file expected lossy                                 |
| `wrong name`       | another codec named, or an encoder family where none is expected       |
| `family`           | the codec found, another encoder family named                          |
| `OVERCLAIM`        | a damaged file read without a limitation                               |
| `unclear`          | a file refused without the expected words                              |
| `CRASH`            | an exit status other than 0, 1 or 2, from any binary or read           |
| `HANG`             | no result within the timeout, from any binary or read                  |

- `FALSE ACCUSATION`, `possibly lossy`, `miss`, `wrong name`, `OVERCLAIM`, `CRASH` and `HANG`
  are the failures that matter most
