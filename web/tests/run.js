// reads one file with the wasm in web/page/wasm and writes what `check` returns to stdout. an error
// `check` throws goes to stderr with exit status 2. a trap is not caught, and deno ends with
// status 1 for it
import { check, initSync } from "../page/wasm/choon_web.js";

/**
 * all of `text` to `to`, which may take fewer bytes than it is given at a time
 * @param {{ writeSync(bytes: Uint8Array): number }} to
 * @param {string} text
 */
function write(to, text) {
  let bytes = new TextEncoder().encode(text);
  while (bytes.length) bytes = bytes.subarray(to.writeSync(bytes));
}

const [file, thresholds = ""] = Deno.args;
initSync({ module: Deno.readFileSync(new URL("../page/wasm/choon_web_bg.wasm", import.meta.url)) });
try {
  write(Deno.stdout, check(Deno.readFileSync(file), thresholds));
} catch (error) {
  if (error instanceof WebAssembly.RuntimeError) throw error;
  write(Deno.stderr, error.message);
  Deno.exitCode = 2;
}
