// reads one file with the wasm in web/page/wasm and writes what `check` returns to stdout. an error
// `check` throws goes to stderr with exit status 1. a trap is not caught, so node ends with
// its own status for an uncaught error
import { readFileSync } from "node:fs";

import { check, initSync } from "../page/wasm/choon_web.js";

const [file, thresholds = ""] = process.argv.slice(2);
initSync({ module: readFileSync(new URL("../page/wasm/choon_web_bg.wasm", import.meta.url)) });
try {
  process.stdout.write(check(readFileSync(file), thresholds));
} catch (error) {
  if (error instanceof WebAssembly.RuntimeError) throw error;
  process.stderr.write(error.message);
  process.exitCode = 1;
}
