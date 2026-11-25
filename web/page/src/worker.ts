// reads one file at a time for the page. posts the export's version once the analyzer is up,
// then for each file posted `{ result }`, the JSON `check` returns, or `{ error }`, why the
// file was not read. a trap is reported as an uncaught error, which the page gets as the
// worker's `error` event
import { check, initSync, version } from "../public/choon_web.js";
import wasm from "../wasm/choon_web_wasm.js";

export type Posted = { version: number } | { result: string } | { error: string };

function post(message: Posted) {
  postMessage(message);
}

// the wasm comes as base64 in a module, as a fetch of it is a connection the page's policy
// refuses
const binary = atob(wasm);
const module = new Uint8Array(binary.length);
for (let at = 0; at < binary.length; at++) module[at] = binary.charCodeAt(at);
initSync({ module });
post({ version: version() });

onmessage = async ({ data: file }: MessageEvent<File>) => {
  let bytes;
  try {
    bytes = new Uint8Array(await file.arrayBuffer());
  } catch {
    // a directory, or a file moved or changed since it was chosen
    post({ error: "the browser could not read the file" });
    return;
  }
  try {
    post({ result: check(bytes, "") });
  } catch (error) {
    // `check` throws a plain `Error` with the library's message. anything else is a trap or
    // memory that could not grow, after which the instance is not used again
    if (error instanceof Error && error.constructor === Error) post({ error: error.message });
    else reportError(error);
  }
};
