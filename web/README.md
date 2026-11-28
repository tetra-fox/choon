# choon-web

The choon analyzer built for WebAssembly, and a page that reads audio files with it in the
browser. The crate is one function that takes a file's bytes and returns its reading as JSON.
It is a workspace of its own and depends on the library with default features off and `serde`
on.

## Build

```sh
nix develop .#web -c just web
```

The page is plain HTML, CSS and JavaScript in `web/page`, with no build tool and no npm
packages. `just web` builds the wasm and copies the page into `web/dist`:

| File                      | Description                                                 |
| ------------------------- | ----------------------------------------------------------- |
| `index.html`              | the page, with its policy, and the commit filled in         |
| `page.css`                | its stylesheet                                              |
| `page.js`                 | the list of files, their readings and what is kept          |
| `worker.js`               | the Web Worker that runs the analyzer                       |
| `wasm/choon_web.js`       | what wasm-bindgen writes for the worker to import           |
| `wasm/choon_web_wasm.js`  | the wasm as base64 in a module                              |

- The wasm uses SIMD (`+simd128`) and goes through `wasm-opt -O2`
- The `wasm-bindgen` crate is pinned in `Cargo.toml`, and the flake takes the version of
  wasm-bindgen's program from that line
- `web/page/wasm` holds what wasm-bindgen writes (`--target web`): `choon_web.js`, its types and
  `choon_web_bg.wasm`, for the worker and for other callers. `just web` writes it, and git
  ignores it
- `index.html` names the commit `web/dist` was built from and links the source at that commit,
  under the `repository` of the main `Cargo.toml`. A build from a checkout with changes says so
- The links in `index.html` are relative, so `web/dist` is served from any path of a host
- The files keep their names from one build to the next. A host has the browser check each
  file again on every visit: Cloudflare sends `max-age=0, must-revalidate` with an `ETag`
- `just web-check` checks the types of `page.js` and `worker.js` from their JSDoc with
  `deno check`, under the settings in `deno.json`

## Page

Any server of static files serves `web/dist`:

```sh
python3 -m http.server --directory web/dist 8000
```

The page takes files dropped on it or chosen in a file picker and reads them one after
another in one Web Worker. Each file is shown with its verdict, what the program prints in
parentheses after one, the reading's summary, the format, the flags and the limitations.
Nothing is loaded from another origin.

- A file is measured in full under the default thresholds
- A file over 256 MiB (`MAX_FILE_BYTES` in `page.js`) is shown as `not read` without being
  opened. The worker holds a file's bytes twice while it copies them into the analyzer's
  memory, the analysis adds to that, and a tab on a phone can be ended at a few hundred MiB.
  The number is an estimate: no phone was measured for it
- A file being read shows for how long, and `Cancel` ends the worker. No progress is shown,
  as `check` reports none
- A trap ends the worker too, and the file is shown as `not read`. The next file gets a new
  worker
- A browser without WebAssembly SIMD is told so and reads nothing

### Storage

The page keeps its readings in `localStorage` under the key `choon.readings`, and nowhere
else.

- An entry is a file's name, size and last-modified time with what `check` returned for it,
  a few kB. A file read again replaces its entry
- Files not read are not kept
- `Clear` removes the readings shown and the key
- An entry whose `version` is not the one the page's wasm writes is dropped when the page
  loads
- When the browser refuses the write (storage full or turned off), the reading is shown with
  a line that it was not kept

### Content Security Policy

`index.html` states its policy in a `meta` tag: scripts and styles from the page's own
origin, an empty icon as a `data:` URL, WebAssembly, a worker from a `blob:` URL whose
modules are the origin's, and no connection of any kind (`connect-src 'none'`).

- The worker is made from a `blob:` URL so that the page's policy holds it too. A worker made
  from its own URL is held to the policy of that response, and a static host sends none
- Chromium 158 and Firefox 160 both refuse a connection from that worker under the `meta`
  policy alone. Safari was not tried
- The worker cannot fetch the wasm under this policy, so it has it as base64 in its own file
- The policy names no hash and no nonce, as `index.html` has no script or style written into
  it
- A host that sends the same policy as a response header changes nothing the page needs

## Usage

```js
import init, { check } from "./choon_web.js";

await init({ module_or_path: new URL("choon_web_bg.wasm", import.meta.url) });
const bytes = new Uint8Array(await file.arrayBuffer());
const { measured, reading } = JSON.parse(check(bytes, ""));
```

`choon_web.js` and `choon_web_bg.wasm` are in `web/page/wasm` after `just web`. `init` is told
where the wasm is, as the module is written without a default path.

`check(bytes: Uint8Array, thresholds: string): string`

- `thresholds` is a profile as the program's `--profile` takes one,
  `{"measure": {...}, "reading": {...}}`. Keys left out keep their default, an unknown key is
  refused, and an empty string means the defaults
- The file is measured in full, and its format is found from its bytes alone
- An error of the library is thrown as an `Error` with the library's message, and so are
  thresholds that are not a profile
- `check` blocks the thread it is called on and takes no deadline, progress or cancel check
- The whole file is held in the instance's memory
- A panic is a trap, which JavaScript catches as a `WebAssembly.RuntimeError`. The caller
  makes a new instance after one: the page starts a new worker

`version(): number` returns the `version` that `check` writes, without reading a file.

## Output

```json
{
  "version": 2,
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
  "words": {
    "verdict": "likely transcode",
    "found": ["MP3 grid", "cutoff 16.8 kHz", "MP3 ~128 kbps"],
    "format": "FLAC, 44100 Hz, 16 bit, 2 channels, 0:34",
    "flags": []
  },
  "reading": { "verdict": "likely_transcode", "summary": "...", "limitations": [] }
}
```

| Key                    | Description                                                   |
| ---------------------- | ------------------------------------------------------------- |
| `version`              | version of this JSON's shape                                  |
| `measured`             | fields of the library's `Measured`, under the names it writes |
| `measured.dsdMultiple` | `Measured::dsd_multiple`, 64 for DSD64                        |
| `measured.windowCount` | number of windows measured                                    |
| `measured.gridScanned` | whether the grid scan ran                                     |
| `words.verdict`        | the verdict as the program prints it                          |
| `words.found`          | what the program prints in parentheses after the verdict      |
| `words.format`         | the `Format` line of the program's readout                    |
| `words.flags`          | the reading's flags as the program prints them                |
| `reading`              | the library's `Reading`, as its `serde` feature writes it     |

- Every key is always there; one without a value is `null`, or `[]` for `opus`
- The example's `reading` is cut to three of its keys; the library's `Reading` has the rest
- The windows and the grid scan of `Measured` are left out, which leaves a few kB
- The wording is the program's module `src/cli/words.rs`, which this crate includes as a
  file, as the library does not export it
- `version` goes up with any change to this shape, a change of the library's to `reading`,
  `claimed`, `opus` or `heAac` included. The library is 0.x and can change those with any
  minor version
