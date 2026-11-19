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

## The manifest

`make.sh` writes `manifest.tsv` beside the files, one line a case, tab-separated:

1. the file
2. what it should read as
3. where that expectation comes from: a section of the README, `samples/README.md`, the
   program's help or a type's documentation
4. how the file was made

| Expectation        | A case passes when                                                         |
| ------------------ | -------------------------------------------------------------------------- |
| `honest`           | the verdict is consistent or inconclusive, with no grid, and the exit is 0 |
| `consistent`       | the verdict is consistent                                                  |
| `lossy:CODEC`      | a grid of CODEC is found; `any` for whichever codec                        |
| `lossy:CODEC:NAME` | and the encoder family is NAME; `none` for no family after a cut start     |
| `atmost`           | it is read; the README says the decode may not be found                    |
| `as:FILE`          | the verdict, grid, family and cutoff are FILE's                            |
| `limited`          | it is not read, or its reading states what was not read                    |
| `refused:WORDS`    | it is not read, and the reason holds WORDS                                 |
| `unread`           | it is not read                                                             |
| `other`            | it is read, and no encoder family is named; the codec is not looked for    |
| `open`             | it is read or refused without a crash; nothing documents which             |

## File names

`<group>-<history>_<bits>-<kHz>.<ext>`, as in `samples/`, where the depth and rate matter.

- `A`: lossless from the start, and must never read as a likely transcode or mixed sources
- `B`: decodes of lossy files, and what was done to them after `-then-`
- `C`: containers, tags, muxers, damage and the edges of each format
- `D`: lengths

## The run

`run.py RESULTS.tsv BINARY...` reads each file alone with each binary as a line, as JSON and
as JSON sampled at 20 windows, with a minute's timeout, and writes the exit status, wall time
and peak memory of every run. The first binary's reading is judged against the expectation.
Another binary that reads the file differently, and a sampled read that does, is noted beside
it. A second binary built with `-C debug-assertions -C overflow-checks` turns an integer
overflow that a release build wraps into a panic.

The outcomes: `as documented`, `worse`, `undocumented`, and by name the failures that matter
most: `FALSE ACCUSATION` (an honest file read as lossy), `CRASH`, `HANG`, `OVERCLAIM` (a
damaged file read without a word of it), `wrong name`, `miss`, `possibly lossy` (an honest
file read so).
