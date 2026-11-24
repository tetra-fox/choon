# choon-web

The choon analyzer built for WebAssembly: one function that takes a file's bytes and returns
its reading as JSON. It is a workspace of its own and depends on the library with default
features off and `serde` on.

## Build

```sh
nix develop .#web -c just web
```

`web/dist` then holds `choon_web_bg.wasm` and the module wasm-bindgen writes for it,
`choon_web.js` (`--target web`), with their TypeScript declarations.

- The wasm uses SIMD (`+simd128`) and goes through `wasm-opt -O2`
- The `wasm-bindgen` crate is pinned in `Cargo.toml`, and the flake takes the version of
  wasm-bindgen's program from that line

## Usage

```js
import init, { check } from "./choon_web.js";

await init();
const bytes = new Uint8Array(await file.arrayBuffer());
const { measured, reading } = JSON.parse(check(bytes, ""));
```

`check(bytes: Uint8Array, thresholds: string): string`

- `thresholds` is a profile as the program's `--profile` takes one,
  `{"measure": {...}, "reading": {...}}`. Keys left out keep their default, an unknown key is
  refused, and an empty string means the defaults
- The file is measured in full, and its format is found from its bytes alone
- An error of the library is thrown as an `Error` with the library's message, and so are
  thresholds that are not a profile
- `check` blocks the thread it is called on and takes no deadline, progress or cancel check
- The whole file is held in the instance's memory
- A panic is a trap, which JavaScript catches as a `WebAssembly.RuntimeError`. An instance
  that trapped is replaced by a new one

## Output

```json
{
  "version": 1,
  "measured": {
    "claimed": { "codec": "flac", "sampleRate": 44100, "bitDepth": 16, "channels": 2, "lossless": true, "frames": 1499136 },
    "duration": 33.99401360544218,
    "bitrateKbps": null,
    "opus": [],
    "heAac": null,
    "dsdRate": null,
    "dsdMultiple": null,
    "sampled": false,
    "windowCount": 34,
    "gridScanned": true
  },
  "reading": { "verdict": "likely_transcode", "summary": "...", "limitations": [] }
}
```

| Key                    | Description                                                     |
| ---------------------- | --------------------------------------------------------------- |
| `version`              | version of this JSON's shape                                    |
| `measured`             | fields of the library's `Measured`, under the names it writes   |
| `measured.dsdMultiple` | `Measured::dsd_multiple`, 64 for DSD64                          |
| `measured.windowCount` | number of windows measured                                      |
| `measured.gridScanned` | whether the grid scan ran                                       |
| `reading`              | the library's `Reading`, as its `serde` feature writes it       |

- Every key is always there; one without a value is `null`, or `[]` for `opus`
- The windows and the grid scan of `Measured` are left out, which leaves a few kB
- `version` goes up with any change to this shape, a change of the library's to `reading`,
  `claimed`, `opus` or `heAac` included. The library is 0.x and can change those with any
  minor version

## Tests

```sh
nix develop .#web -c just web-test
```

`tests/node.rs` writes generated audio to a temporary directory, has node read each file with
the wasm in `web/dist` (`tests/run.mjs`), and compares the JSON with what the same function
returns natively. `just ci` does not run it.

- Files compared: FLAC, FLAC decoded from MP3 and from Opus, MP3, Opus in Ogg, WebM and MP4,
  FLAC in Ogg, WAV, RF64, AIFF, CAF, WavPack, Monkey's Audio, DSF, DSDIFF, ALAC, HE-AAC in MP4
  and ADTS, HE-AAC v2 and MP2. Each opens from its bytes with no file name
- Everything but the floats is the same: the verdicts, a grid's codec, family and offset, the
  flags, the summary and the limitations
- The floats differ by at most 6.1e-5 of their value: 2.6e-16 in a lossless file with no
  grid, 1.8e-6 in the two with one, and the most in the MP2 file, where a shelf of 91.29 dB
  reads 0.006 dB lower. The test allows 1e-3
- Vorbis and AAC-LC are not compared, as nothing here encodes them
- The two error cases, bytes that are not audio and a hybrid WavPack file, throw the message
  the library gives
- No file here makes the wasm trap. ape-decoder's panic on a damaged Monkey's Audio file is an
  overflow check, which a release build leaves out: the two damaged files saved in
  `proptest-regressions/tests/hostile.txt` give a reading in the release wasm
