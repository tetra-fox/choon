# Supported formats

The [README](../README.md#supported-formats) lists the codecs and containers read.

## Refused files

| Format         | Refused                                                             |
| -------------- | ------------------------------------------------------------------- |
| WavPack        | a hybrid file, which is lossy without its correction file           |
| WavPack        | 8-bit samples, more than two channels, streams older than WavPack 4 |
| Monkey's Audio | files older than version 3.99, float samples, 32-bit samples        |
| DSDIFF         | DST compression                                                     |

- Each is refused by name

## Files

- Of several audio tracks in a file the first is read, and of the streams chained in an Ogg
  file the first. The reading says so of both
- No audio is read of an MP4 file cut short

## PCM, WAV and FLAC

- PCM is read at 8, 16, 24 and 32 bits and as 32 and 64-bit float
- A-law, mu-law and ADPCM in a WAV file are read as the lossy codecs they are
- RF64 files are read, and Wave64 files are not
- A WAV file is as long as its bytes when it states no size of its samples (0 or all ones, as
  a program writing to a pipe leaves it) or wrapped past 4 GB
- ID3v1, Lyrics3 and APE tags after a FLAC file are left out
- Other bytes after a FLAC file cost its last frame, and the reading says the last packet is
  cut short or followed by bytes that are no audio

## MP3 and AAC

- MP3 is read as MPEG-1, MPEG-2 and MPEG-2.5 Layer III, from 8 kHz up
- A free-format MP3 is not read
- The length of an MP3 with no Xing or VBRI header is a guess from its bitrate, and the file
  is read to its last frame whatever the guess
- HE-AAC is decoded at its output rate, with spectral band replication and parametric stereo
- The band above an HE-AAC core is the decoder's reconstruction, so its reading names no encoder
- AAC-LD and AAC-ELD are read too, each tried on one file from fdk-aac

## Opus

- Opus is read in Ogg (`.opus`, `.ogg`), Matroska/WebM (`.mka`, `.webm`) and MP4, with the
  coding mode and audio bandwidth its packets state
- An Opus file starts after its pre-skip and has its header's output gain applied
- Matroska does not say where the last Opus packet's audio ends, so up to 20 ms of padding is
  read with it

## WavPack and Monkey's Audio

- WavPack is read as mono or stereo at 16, 24 or 32 bits or as float
- Monkey's Audio is read at 8, 16 or 24 bits from version 3.99 on

## DSD

- DSD is measured as PCM at a sixteenth of its rate and at most 352.8 or 384 kHz: 176.4 kHz
  for DSD64
- The DSD filter passes 0.45 of that rate and removes what would fold onto it by 115 dB
- A DSD reading says where the band stops, which shows a source limited like 44.1 or 48 kHz
  PCM or cut by a lossy encoder
- A DSD reading cannot tell a DSD recording from PCM converted up, and the noise above the
  audio band reads as content
