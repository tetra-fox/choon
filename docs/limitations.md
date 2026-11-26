# Limitations

The [README](../README.md#limitations) lists the limits of the cutoff reading and of the
files read.

## Frame grid

- The grid is scanned in a file whose length is known: one that states it, and a FLAC or WAV
  file written to a pipe once it has been read to its end
- FLAC in Matroska and audio read from a pipe are not scanned, and a decode in one reads as
  possibly lossy at most
- An MP3 or AAC decode converted to another rate is looked for between 44.1 and 48 kHz only;
  after any other conversion it can read as possibly lossy
- A Vorbis decode whose grid does not stand out under Vorbis's own window can read as AAC,
  without AAC's quantizer steps, and a WMA decode's can too
- An AAC decode coded mostly in short blocks, as FAAC's can be, can read as AC-3 where AAC's
  quantizer steps do not show
- The encoder named beside a grid is the family its steps or offset fit, as
  [encoders.md](encoders.md#encoder-family) counts: an encoder the bounds were not fitted on
  can read as one of those that were, and a cut start can land on another encoder's offset
- The grid thresholds were tuned on the encoders the [README](../README.md#results) counts;
  [encoders.md](encoders.md#tested-encoders) lists what the scan finds of others
- A lossy sample that was processed and mixed into a master, as a pitched or filtered clip
  under other sounds, leaves no grid and is not found

## Lossy sections, clipping and quiet audio

- A lossy section shows a grid only in the stretches that fall in it
- One in a single stretch is found by its grid only at the offset of a decode whose start was
  not cut, and one between the four by its cutoff alone, as possibly lossy at most
- Clipping hides the grid frame by frame, so a decode made louder until it clips can lose it
- The scan reads stretches that are not clipped where the file has them. A file clipped
  throughout can read as consistent, with a limitation that says the stretches scanned were
  clipped
- A quiet passage coded at a high bitrate shows little of a grid
- The scan reads the loudest stretch in the place of one under -20 dBFS and over 10 dB quieter,
  and a file quiet throughout can still read as consistent

## Opus

- Opus is looked for at 44.1 and 48 kHz only
- Frames the pitch post-filter ran over are found only where nothing was cut from the decode's
  start; elsewhere a decode made mostly of such frames reads as possibly lossy at most, as does
  one in SILK voice mode
- A decode of frames under 20 ms whose level was changed afterwards is not found, and neither
  is a hybrid-mode decode whose level was changed
- A level that changes over time after an Opus decode, as under a fade or a limiter, hides the
  lattice where it changes
- An Opus decode folded to mono, with one channel copied to both, turned to mid and side, or
  truncated to 8 bits is not found
