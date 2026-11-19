# Sample files

Audio to try `choon` on: pink noise from ffmpeg, encoded by each encoder's own program and
decoded again to FLAC. No recording is used, and the files are not kept in the repository.

```sh
just samples
choon check samples/
```

`just samples` runs `make.sh` in a nix shell that brings ffmpeg, LAME, opus-tools, vorbis-tools
and fdkaac, and writes about 50 MB here. Without nix, `samples/make.sh` runs with those five
programs installed.

## File names

`<history>_<bits>-<kHz>.<ext>`

- `<history>` is what happened to the audio before it was written: the program that encoded
  it and its setting (`lame-128`, `oggenc-q6`), or what was done to lossless audio (`cut-17k`)
- A later step follows `-then-`: `lame-128-then-48k` was decoded, then converted to 48 kHz
- `<bits>-<kHz>` is the bit depth and sample rate the file is written at
- A lossy file in its own format has only its history: `lame-128.mp3`

## What each reads as

| File                              | History                                          | Reading                               |
| --------------------------------- | ------------------------------------------------ | ------------------------------------- |
| `lossless_16-44.flac`             | never lossy                                      | consistent                            |
| `lame-128_16-44.flac`             | `lame -b 128`, decoded                           | likely transcode, MP3 grid, LAME      |
| `lame-320_16-44.flac`             | `lame -b 320`, decoded                           | likely transcode, MP3 grid, LAME      |
| `lame-128-then-48k_16-48.flac`    | `lame -b 128`, decoded, converted to 48 kHz      | likely transcode, MP3 grid at 44.1    |
| `lame-128-intro_16-44.flac`       | 12 s from `lame -b 128`, then lossless audio     | possibly lossy, mixed sources         |
| `ffmpeg-aac-128_16-44.flac`       | ffmpeg's AAC encoder at 128 kbps, decoded        | likely transcode, AAC grid            |
| `fdkaac-128_16-44.flac`           | `fdkaac -b 128`, decoded                         | likely transcode, AAC grid            |
| `opusenc-128_16-48.flac`          | `opusenc --bitrate 128`, decoded                 | likely transcode, Opus grid, libopus  |
| `opusenc-128-then-44k_16-44.flac` | `opusenc --bitrate 128`, decoded, to 44.1 kHz    | likely transcode, Opus grid, libopus  |
| `oggenc-q6_16-44.flac`            | `oggenc -q 6` (Vorbis), decoded                  | likely transcode, read as an AAC grid |
| `cut-17k_16-44.flac`              | lossless audio filtered at 17 kHz                | possibly lossy, no grid               |
| `dual-mono_16-44.flac`            | one channel copied to both                       | consistent, flagged dual mono         |
| `lame-128.mp3`                    | `lame -b 128`, as the MP3                        | consistent with its bitrate           |
| `ffmpeg-aac-128.m4a`              | ffmpeg's AAC encoder at 128 kbps, as the M4A     | consistent with its bitrate           |
| `opusenc-128.opus`                | `opusenc --bitrate 128`, as the Opus file        | consistent with its bitrate           |

The readings are those of files written by ffmpeg 9.0.2, LAME 4.0, opus-tools 0.2 with libopus
1.6.1, vorbis-tools 1.4.3 and fdkaac 1.0.9.
