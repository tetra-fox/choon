# Encoders

## Encoder family

A grid's encoder family is named two ways. The steps name it wherever the grid sits; the offset
names it in a decode whose start was not cut.

### Quantizer steps

An MP3 or AAC encoder sets a step for each scalefactor band, and encoders differ in how often,
how far and which way they move it from one band to the next. Where the steps of a decode show,
shares of its bands and neighbouring bands are held against bounds fitted to each encoder:

| Codec | Family     | Steps                                                                 |
| ----- | ---------- | --------------------------------------------------------------------- |
| MP3   | LAME       | many steps moved, some far, mid and side in use                       |
| MP3   | Helix      | many steps two quarter powers coarser than the band under, few far    |
| MP3   | Fraunhofer | few bands at the coarsest step of their granule                       |
| MP3   | Shine      | one step for every band of a granule, with small values               |
| AAC   | ffmpeg     | few neighbouring bands with one step                                  |
| AAC   | fdk-aac    | half or more neighbouring bands with one step, the rest close         |

- A decode is named where the bounds of one family alone hold, and stays unnamed otherwise
- `choon check --defaults` prints the bounds, as `mp3FamilySteps` and `aacFamilySteps`
- A decode with fewer than 2,000 pairs of neighbouring bands on whole steps is not named: a
  clipped or very quiet file leaves few, and their shares stray
- The bounds were tested on 26 recordings none of them was placed on: classical, electronic,
  ambient, jazz, live, clipped and near-mono ones among them, two-minute excerpts through each
  encoder, every decode read whole and with 1,009 to 41,017 samples cut from its start

| Encoder                             | Named by steps, start cut | Named by steps, whole | Named as another's |
| ----------------------------------- | ------------------------- | --------------------- | ------------------ |
| LAME 3.97 to 3.100, CBR and VBR     | 185 of 650                | 188 of 650            | 0                  |
| LAME 3.90.3 and 3.93.1, alt-presets | 43 of 104                 | 45 of 104             | 0                  |
| LAME 3.90.3 and 3.93.1, CBR         | 0 of 156                  | 0 of 156              | 0                  |
| Helix                               | 137 of 156                | 137 of 156            | 0                  |
| Fraunhofer, the codec Windows ships | 145 of 156                | 137 of 156            | 0                  |
| Shine at 96 to 192 kbps             | 69 of 78                  | 72 of 78              | 0                  |
| Shine at 320 kbps                   | 1 of 26                   | 1 of 26               | 0                  |
| BladeEnc, GOGO                      | 0 of 260                  | 0 of 260              | 0                  |
| ffmpeg's AAC                        | 112 of 130                | 111 of 130            | 0                  |
| fdk-aac                             | 141 of 208                | 136 of 208            | 0                  |
| Nero, FAAC                          | 0 of 130                  | 0 of 130              | 0                  |

- A whole LAME, Helix or Nero decode that its steps leave unnamed is named by its offset
- The bounds were placed on the decodes of 200 other recordings: 100 through LAME, Helix,
  Shine, BladeEnc, GOGO, fdk-aac and Nero, 60 of them through Fraunhofer's codec and ffmpeg's
  AAC and 40 through Apple's MP3 encoder, and the corpus's LAME and ffmpeg AAC decodes of 100
  more. Each bound stands 0.02 clear of every decode of another encoder among them that meets
  its rule's other bounds, and Shine's two stand 0.001 and 0.1 clear
- Of those decodes with their starts cut, the bounds name 871 of 3,294 of LAME, 524 of 600 of
  Helix, 329 of 360 of Fraunhofer, 230 of 360 of Shine, 257 of 300 of ffmpeg's AAC and 466 of
  640 of fdk-aac, and none of 840 of BladeEnc and GOGO or 120 of Apple's MP3 encoder
- LAME's bounds are drawn against GOGO, which was made from LAME's source, and against LAME
  3.90.3 and 3.93.1 at a constant bitrate, which move their steps as GOGO does. They stand
  0.05 clear of GOGO's decodes, name about a quarter of LAME's decodes after a cut, and none of
  those two
- How many of a recording's LAME decodes are named varies with the recording, from none to all
- Nero's steps lie beside fdk-aac's at 256 kbps. Bounds placed for Nero named 6 fdk-aac decodes
  of the 26 recordings as Nero's, so Nero has no bounds and is named by its offset alone
- Every encoder leaves the steps alone in some decodes at 256 kbps and up, where the values
  are large, so Shine is named only where its values are small
- Apple's AAC encoder and FAAC use the sine window, which the AAC bounds are not read under
- Other versions of Fraunhofer's encoder were not tested

### Offset and window

An encoder delays its input by a fixed number of samples, and an AAC encoder keeps to one window
for its long blocks, so a grid's offset and window fit an encoder family:

| Codec | Offset | Window        | Family                            |
| ----- | ------ | ------------- | --------------------------------- |
| MP3   | 48     |               | LAME                              |
| MP3   | 96     |               | Helix                             |
| MP3   | 1      |               | an encoder that states no delay   |
| AAC   | 960    | sine          | Apple                             |
| AAC   | 0      | sine          | FAAC                              |
| AAC   | 448    | Kaiser-Bessel | Nero, where AAC's steps show      |
| AAC   | 0      | Kaiser-Bessel | ffmpeg or fdk-aac                 |
| Opus  | 228    |               | libopus                           |

- The offsets are those of a decode whose start was not cut, from a decoder that cuts the delay
  the file states, as ffmpeg does
- Shine, BladeEnc, GOGO, Apple's and Fraunhofer's MP3 encoders state no delay, and a LAME file
  decoded without that cut sits at offset 1 too
- The steps' name stands before the offset's for MP3. A cut start that lands on another
  encoder's offset names a decode wrongly where its steps name nothing: 7 of the 5,574 cut MP3
  decodes the bounds were placed on
- Versions of one encoder share its delay and read alike
- About one Vorbis decode in thirteen reads as AAC at Nero's offset, without AAC's steps, so
  Nero is named only where those steps show
- Opus frames under 20 ms sit at 468 or 180 (10 ms), 108 or 60 (5 ms) and 48 or 0 (2.5 ms),
  by libopus's two delays

### Setting

The program names the family after the grid's codec and the setting after it, as
`MP3 (LAME) ~128 kbps`, `AAC (fdk-aac) ~128-160 kbps` or `AAC (Apple)`. The setting is the one
whose cutoff most of the file stops at with a sharp shelf:

| Family           | Cutoffs                                                          |
| ---------------- | ---------------------------------------------------------------- |
| LAME             | the profiles of `choon check --defaults`                         |
| ffmpeg's AAC     | 16.0 kHz at 96 kbps, 17.25 at 128, 17.96 at 160, 19.27 at 192    |
| fdk-aac          | 14.31 kHz at 96 kbps, 16.0 at 128 and 160, 17.02 at 192 and 256  |
| fdk-aac, VBR     | 16.53 kHz in mode 4, 19.19 in mode 5; mode 3 cuts at 16.0        |

- An MP3 grid of no named encoder, or of one that states no delay, takes LAME's setting
- LAME 3.90.3 and 3.93.1 cut 128 kbps at 15.2 kHz, where later versions cut 96, so LAME's
  setting there is `~96-128 kbps`. GOGO cuts 128 kbps there too, 160 at 18.5 kHz and 192 at
  18.9, and reads as LAME's `~96`, `~192` and `~224 kbps`
- Helix cuts at 16 kHz at every bitrate unless told otherwise, Fraunhofer's codec moves its
  cutoff with the audio, and Shine does not cut, so no setting is named beside them
- Of the whole decodes of the 26 recordings, 32 of 130 of ffmpeg's AAC show a setting, each its
  own. Of 208 of fdk-aac 80 do: 65 their own and 15 in mode 3 the `~128-160 kbps` it shares

## Tested encoders

Decodes to FLAC of 40 recordings per setting, at each encoder's common bitrates:

| Codec  | Encoder                                          | Decodes found, of 40 |
| ------ | ------------------------------------------------ | -------------------- |
| MP3    | LAME 3.90.3, 3.93.1, 3.97, 3.98.4, 3.99.5, 3.100 | 38 to 40             |
| MP3    | Helix, Shine, BladeEnc, GOGO                     | 39 to 40             |
| MP3    | Apple (Music) at 160, 320 and VBR                | 39 to 40             |
| AAC-LC | ffmpeg, fdk-aac                                  | 40                   |
| AAC-LC | Nero, Apple (CBR, constrained VBR, true VBR)     | 39 to 40             |
| AAC-LC | FAAC at 128 kbps and at quality 100              | 30 and 36            |
| AAC-LC | FAAC at 256 kbps                                 | 5                    |
| HE-AAC | Apple, fdk-aac, Nero at 48 and 64 kbps           | 4 to 21              |
| HE-AAC | Apple, fdk-aac, Nero, v2 at 32 kbps              | 0 to 3               |
| Vorbis | libvorbis 1.1.2 to 1.3.7, aoTuV, at q3 and q6    | 34 to 36             |
| Vorbis | libvorbis 1.2.0 and 1.3.7, aoTuV, at q9          | 18 to 23             |
| Opus   | libopus 1.1, 1.3.1, 1.5.2, 1.6.1 at 64 to 256    | 39                   |

- FAAC at 256 kbps codes nearly every frame as short blocks, which the scan does not analyse
- An HE-AAC decode's core was coded at half the sample rate under the decoder's filterbank
- The Vorbis rows were counted by the AAC scan, before the scan ran under Vorbis's own window
- AAC's quantizer steps show in 828 of the 880 AAC-LC decodes, 6 of which the count misses, in
  none of the 680 Vorbis decodes and none of the 280 HE-AAC ones
- Fraunhofer's MP3 encoder, as the codec Windows ships, was checked on 44 other recordings at
  96 to 320 kbps: every decode is found, at offset 1
- An AAC decode coded mostly in short blocks counts under AC-3's shorter transform. Where AAC's
  steps show it reads as AAC all the same; of 240 FAAC decodes of the 60 recordings above, 23
  whose steps do not show read as AC-3
