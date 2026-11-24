// reads the files of a directory on the page of web/dist in a headless Chromium, over its
// DevTools protocol, and ends with status 1 at the first thing that is not as it should be.
// `tests/page.rs` writes the files and runs this. with SHOTS set to a directory, screenshots
// of the page are written there
import { spawn } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { extname, join } from "node:path";

const dist = new URL("../dist/", import.meta.url);
const [files] = process.argv.slice(2);
const shots = process.env.SHOTS;

const TYPES = {
  ".html": "text/html",
  ".css": "text/css",
  ".js": "text/javascript",
  ".svg": "image/svg+xml",
  ".wasm": "application/wasm",
};

// the page's own policy, which the second server sends as a header too
const policy = readFileSync(new URL("index.html", dist), "utf8").match(
  /Content-Security-Policy"\s+content="([^"]+)"/,
)[1];

// a server of web/dist's files on a port of its own, with the policy as a header when
// `header`, which binds the worker in a browser that takes a worker's policy from its response
const servers = [];
function serve(header) {
  const server = createServer((request, response) => {
    const name = request.url === "/" ? "index.html" : request.url.slice(1);
    let body;
    try {
      body = readFileSync(new URL(name, dist));
    } catch {
      response.writeHead(404).end();
      return;
    }
    const headers = { "content-type": TYPES[extname(name)] };
    if (header) headers["content-security-policy"] = policy;
    response.writeHead(200, headers).end(body);
  });
  return new Promise((resolve) => {
    servers.push(server);
    server.listen(0, "127.0.0.1", () => {
      resolve(`http://127.0.0.1:${server.address().port}`);
    });
  });
}

function check(holds, what) {
  if (!holds) throw new Error(what);
}

const profile = mkdtempSync(join(tmpdir(), "choon-page-"));
const browser = spawn(
  process.env.CHROMIUM ?? "chromium",
  ["--headless=new", "--remote-debugging-port=0", `--user-data-dir=${profile}`, "about:blank"],
  { stdio: ["ignore", "ignore", "pipe"] },
);
const endpoint = await new Promise((resolve, reject) => {
  let said = "";
  browser.stderr.on("data", (chunk) => {
    said += chunk;
    const found = said.match(/DevTools listening on (ws:\S+)/);
    if (found) resolve(found[1]);
  });
  browser.on("error", reject);
  browser.on("exit", () => reject(new Error(`chromium ended: ${said}`)));
});

const socket = new WebSocket(endpoint);
await new Promise((resolve) => socket.addEventListener("open", resolve));
let sent = 0;
const waiting = new Map();
// every request any target makes, every violation of the policy and every error logged
const requests = [];
const faults = [];
socket.addEventListener("message", ({ data }) => {
  const message = JSON.parse(data);
  if (message.id !== undefined) {
    const { resolve, reject } = waiting.get(message.id);
    waiting.delete(message.id);
    if (message.error) reject(new Error(message.error.message));
    else resolve(message.result);
    return;
  }
  const { method, params } = message;
  if (method === "Target.attachedToTarget") {
    // a worker: its requests are counted with the page's
    worker = params.sessionId;
    void call("Network.enable", {}, worker);
  } else if (method === "Network.requestWillBeSent") {
    requests.push(params.request.url);
  } else if (method === "Log.entryAdded" && params.entry.level === "error") {
    faults.push(params.entry.text);
  } else if (method === "Runtime.exceptionThrown") {
    faults.push(params.exceptionDetails.exception?.description ?? params.exceptionDetails.text);
  } else if (method === "Audits.issueAdded") {
    faults.push(JSON.stringify(params.issue));
  }
});

function call(method, params = {}, sessionId = page) {
  const id = ++sent;
  socket.send(JSON.stringify({ id, method, params, sessionId }));
  return new Promise((resolve, reject) => waiting.set(id, { resolve, reject }));
}

let page;
// the worker started last
let worker;
const { targetId } = await call("Target.createTarget", { url: "about:blank" });
page = (await call("Target.attachToTarget", { targetId, flatten: true })).sessionId;
for (const domain of ["Page", "Network", "Log", "Runtime", "Audits"]) await call(`${domain}.enable`);
await call("Target.setAutoAttach", {
  autoAttach: true,
  waitForDebuggerOnStart: false,
  flatten: true,
});

async function evaluate(expression) {
  const { result, exceptionDetails } = await call("Runtime.evaluate", {
    expression,
    returnByValue: true,
    awaitPromise: true,
  });
  check(!exceptionDetails, `${expression}: ${exceptionDetails?.text}`);
  return result.value;
}

async function open(url) {
  await call("Page.navigate", { url });
  await until(() => evaluate("document.readyState === 'complete'"), `${url} loads`);
}

async function until(holds, what, seconds = 120) {
  const end = Date.now() + seconds * 1000;
  while (Date.now() < end) {
    if (await holds()) return;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error(`timed out waiting until ${what}`);
}

// the rows of the list, each its status, name and the text under them
function rows() {
  return evaluate(`[...document.querySelectorAll("#readings li")].map((row) => ({
    status: row.querySelector(".status").textContent,
    name: row.querySelector(".name").textContent,
    found: row.querySelector(".found")?.textContent ?? "",
    text: row.textContent,
  }))`);
}

async function row(name) {
  return (await rows()).find((row) => row.name === name);
}

async function give(...names) {
  const { result } = await call("Runtime.evaluate", {
    expression: "document.querySelector('#files')",
  });
  await call("DOM.setFileInputFiles", {
    objectId: result.objectId,
    files: names.map((name) => join(files, name)),
  });
}

function click(selector) {
  return evaluate(`document.querySelector(${JSON.stringify(selector)}).click()`);
}

async function shot(name, { width, height, dark }) {
  if (!shots) return;
  await call("Emulation.setDeviceMetricsOverride", {
    width,
    height,
    deviceScaleFactor: 2,
    mobile: width < 500,
  });
  await call("Emulation.setEmulatedMedia", {
    features: [{ name: "prefers-color-scheme", value: dark ? "dark" : "light" }],
  });
  const { cssContentSize } = await call("Page.getLayoutMetrics");
  const { data } = await call("Page.captureScreenshot", {
    captureBeyondViewport: true,
    clip: { x: 0, y: 0, width, height: Math.max(height, cssContentSize.height), scale: 1 },
  });
  writeFileSync(join(shots, `${name}.png`), Buffer.from(data, "base64"));
}

const DESKTOP = { width: 1280, height: 800 };
const PHONE = { width: 390, height: 844 };

async function run() {
  const plain = await serve(false);
  await open(plain);
  check(await evaluate("document.querySelector('#read').hidden"), "no readings are listed at first");
  check(!(await evaluate("document.querySelector('#choose').disabled")), "SIMD was found");
  await shot("empty-desktop-light", DESKTOP);
  await shot("empty-phone-dark", { ...PHONE, dark: true });

  // four files at once are read one after another, top to bottom
  await give("decoded.flac", "source.mp3", "words.txt", "honest.flac");
  await until(async () => (await row("decoded.flac")).status === "reading", "the first file is read");
  check(
    (await rows()).slice(1).every((row) => row.status === "waiting"),
    "the other files wait their turn",
  );
  await until(async () => (await row("decoded.flac")).found.match(/^[1-9]\d* s/), "the time is shown");
  await shot("reading-desktop-light", DESKTOP);
  await shot("reading-phone-light", PHONE);
  await until(async () => (await row("honest.flac")).status === "reading", "the last file is read");

  const decoded = await row("decoded.flac");
  check(decoded.status === "likely transcode", `a FLAC from an MP3 reads as ${decoded.status}`);
  check(decoded.found.startsWith("MP3 grid, cutoff "), `its detail is ${decoded.found}`);
  check(decoded.text.includes("Format") && decoded.text.includes("flac, 44100 Hz"), decoded.text);
  check((await row("source.mp3")).status === "consistent", "the MP3 reads as consistent");
  const words = await row("words.txt");
  check(words.status === "not read" && words.found.length > 0, `words are ${words.status}`);

  // a cancel ends the worker, and the next file is read by another
  await click("#readings li:last-child button");
  const honest = await row("honest.flac");
  check(honest.status === "not read" && honest.found === "canceled", "the canceled file is not read");
  await give("source.mp3");
  await until(async () => (await rows())[0].status === "consistent", "a file is read after a cancel");
  check((await rows()).length === 4, "a file read again takes the place of its row");

  for (const [name, look] of [
    ["readings-desktop-light", DESKTOP],
    ["readings-desktop-dark", { ...DESKTOP, dark: true }],
    ["readings-phone-light", PHONE],
    ["readings-phone-dark", { ...PHONE, dark: true }],
  ]) {
    await shot(name, look);
  }

  // a second visit shows the files read, and none of the files not read
  await open(plain);
  await until(async () => (await rows()).length === 2, "the readings kept are shown again");
  const kept = await rows();
  check(
    kept[0].name === "source.mp3" && kept[0].status === "consistent",
    "the MP3's reading is remembered",
  );
  check(
    kept[1].name === "decoded.flac" && kept[1].found === decoded.found,
    "the FLAC's reading is remembered",
  );

  // a reading kept under another version of the export is dropped
  await evaluate(`{
    const kept = JSON.parse(localStorage.getItem("choon.readings"));
    kept[0].result.version = 0;
    localStorage.setItem("choon.readings", JSON.stringify(kept));
  }`);
  await open(plain);
  await until(async () => (await rows()).length === 1, "the reading of another version is dropped");
  check(
    (await evaluate("JSON.parse(localStorage.getItem('choon.readings')).length")) === 1,
    "and is out of the storage",
  );

  await click("#clear");
  check(await evaluate("document.querySelector('#read').hidden"), "Clear empties the list");
  check((await evaluate("localStorage.getItem('choon.readings')")) === null, "and the storage");
  await open(plain);
  await evaluate("new Promise((resolve) => setTimeout(resolve, 1000))");
  check((await rows()).length === 0, "nothing is shown after Clear");

  // storage that refuses a write costs the remembering alone
  await evaluate("Storage.prototype.setItem = () => { throw new DOMException('full') }");
  await give("source.mp3");
  await until(async () => (await rows())[0]?.status === "consistent", "a file is read without storage");
  check((await rows())[0].text.includes("Keptno, "), "the reading says it was not kept");

  // the page's policy holds the worker too, with no header from the server
  const outcome = await call(
    "Runtime.evaluate",
    {
      expression: `fetch(${JSON.stringify(`${plain}/page.css`)}).then(() => "fetched", () => "refused")`,
      awaitPromise: true,
      returnByValue: true,
    },
    worker,
  );
  check(outcome.result.value === "refused", `a fetch in the worker is ${outcome.result.value}`);
  check(faults.length > 0, "the refused fetch is among the faults");
  faults.length = 0;

  // under the policy as a header, which binds the worker too, the page reads a file all the same
  const strict = await serve(true);
  await open(strict);
  await give("source.mp3");
  await until(async () => (await rows())[0]?.status === "consistent", "a file is read under the header");

  const origins = [plain, strict];
  // the worker's blob is the page's own, and is no request to a server
  const elsewhere = requests.filter(
    (url) =>
      url !== "about:blank" &&
      !origins.some((origin) => url.replace(/^blob:/, "").startsWith(`${origin}/`)),
  );
  check(elsewhere.length === 0, `requests left the origin: ${elsewhere.join(", ")}`);
  check(requests.some((url) => url.endsWith("/worker.js")), "the worker's requests are counted");
  check(faults.length === 0, `errors or policy violations: ${faults.join("; ")}`);
}

try {
  await run();
} catch (error) {
  console.error(error.message);
  if (faults.length) console.error(`the browser logged: ${faults.join("; ")}`);
  process.exitCode = 1;
} finally {
  socket.close();
  browser.removeAllListeners("exit");
  browser.kill();
  await new Promise((resolve) => browser.on("exit", resolve));
  rmSync(profile, { recursive: true, force: true });
  for (const server of servers) {
    server.close();
    server.closeAllConnections();
  }
}
