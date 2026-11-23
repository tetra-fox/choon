# Encoders

## Encoder family

An encoder delays its input by a fixed number of samples, and an AAC encoder keeps to one window
for its long blocks, so a grid's offset and window fit an encoder family:

| Codec | Offset | Window        | Family                              |
| ----- | ------ | ------------- | ----------------------------------- |
| MP3   | 48     |               | LAME                                |
| MP3   | 96     |               | Helix                               |
| MP3   | 1      |               | Shine, BladeEnc, GOGO or Apple      |
| MP3   | 1      |               | Fraunhofer, where its steps show it |
| AAC   | 960    | sine          | Apple                               |
| AAC   | 0      | sine          | FAAC                                |
| AAC   | 448    | Kaiser-Bessel | Nero, where AAC's steps show        |
| AAC   | 0      | Kaiser-Bessel | ffmpeg or fdk-aac                   |
| Opus  | 228    |               | libopus                             |

- The offsets are those of a decode whose start was not cut, from a decoder that cuts the delay
  the file states, as ffmpeg does
- A LAME file decoded without that cut sits at offset 1
- Versions of one encoder share its delay and read alike
- Fraunhofer's encoder is told from the other four at offset 1 by how it moves the quantizer's
  step from one scalefactor band to the next
- About one Vorbis decode in thirteen reads as AAC at Nero's offset, without AAC's steps, so
  Nero is named only where those steps show
- Opus frames under 20 ms sit at 468 or 180 (10 ms), 108 or 60 (5 ms) and 48 or 0 (2.5 ms),
  by libopus's two delays
- The program names the family after the grid's codec, as `MP3 (LAME) ~128 kbps` or
  `AAC (Apple)`
- The setting is the one the cutoff matches. The cutoffs are LAME's, so it is left out beside
  another named encoder and beside a grid of another codec

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
- The weights that name Fraunhofer's encoder were fitted on that codec. They name 361 of 445 of
  its decodes, none of 468 decodes of Shine, BladeEnc, GOGO and Apple, and 3 of 2,521 LAME and
  Helix decodes, which sit at their own offsets
- Other versions of Fraunhofer's encoder were not tested
