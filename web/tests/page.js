// reads the files of a directory on the page of web/dist in a headless browser, `chromium`
// over its DevTools protocol or `firefox` over WebDriver BiDi, and ends with status 1 at the
// first thing that is not as it should be. `tests/page.rs` writes the files and runs this
// under deno. with SHOTS set to a directory, Chromium's screenshots of the page are written
// there
const dist = new URL("../dist/", import.meta.url);
const [files, engine] = Deno.args;
const shots = Deno.env.get("SHOTS");

const TYPES = {
  html: "text/html",
  css: "text/css",
  js: "text/javascript",
};

// the worker's module
const WORKER = /\/worker\.js$/;

// the page's own policy, which the second server sends as a header too
const policy = Deno.readTextFileSync(new URL("index.html", dist)).match(
  /Content-Security-Policy"\s+content="([^"]+)"/,
)[1];

// a server of web/dist's files on a port of its own, as `{ origin, probed }`. with `header`
// it sends the policy as a header too, which binds a worker in a browser that takes a
// worker's policy from its response. with `probe` its worker asks for `/probe` as it starts,
// and `probed()` says whether that request came. with `loose` the page's policy allows
// connections to its origin
const servers = [];
function serve({ header = false, probe = false, loose = false } = {}) {
  let probed = false;
  const server = Deno.serve({ hostname: "127.0.0.1", port: 0, onListen() {} }, (request) => {
    const path = new URL(request.url).pathname;
    const name = path === "/" ? "index.html" : path.slice(1);
    if (name === "probe") {
      probed = true;
      return new Response(null, { status: 204 });
    }
    let body;
    try {
      body = Deno.readTextFileSync(new URL(name, dist));
    } catch {
      return new Response(null, { status: 404 });
    }
    if (probe && WORKER.test(path)) {
      body += '\nfetch(new URL("/probe", import.meta.url)).catch(() => {});\n';
    }
    if (loose && name === "index.html") {
      body = body.replace("connect-src 'none'", "connect-src 'self'");
    }
    const headers = { "content-type": TYPES[name.split(".").pop()] };
    if (header) headers["content-security-policy"] = policy;
    return new Response(body, { headers });
  });
  servers.push(server);
  return { origin: `http://127.0.0.1:${server.addr.port}`, probed: () => probed };
}

function check(holds, what) {
  if (!holds) throw new Error(what);
}

const profile = Deno.makeTempDirSync({ prefix: "choon-page-" });
// every request the page or its worker makes, and every error, exception and policy
// violation the browser reports
const requests = [];
const faults = [];

// the browser started with `args`, and the WebSocket address it says on stderr after `said`.
// its stderr is read to the end, so that a full pipe never stops it
async function launch(program, args, said) {
  const browser = new Deno.Command(program, {
    args,
    stdin: "null",
    stdout: "null",
    stderr: "piped",
  }).spawn();
  const address = await new Promise((resolve, reject) => {
    let heard = "";
    (async () => {
      for await (const chunk of browser.stderr.pipeThrough(new TextDecoderStream())) {
        heard += chunk;
        const found = heard.match(said);
        if (found) resolve(found[1]);
      }
      reject(new Error(`${program} ended: ${heard}`));
    })().catch(reject);
  });
  return { browser, address };
}

// a WebSocket to `address` as `call(method, params, extra)`, which sends a command with
// `extra` beside its id and gives its result, with every other message passed to `event`
async function connect(address, event) {
  const socket = new WebSocket(address);
  await new Promise((resolve, reject) => {
    socket.addEventListener("open", resolve);
    socket.addEventListener("error", () => reject(new Error(`no connection to ${address}`)));
  });
  let sent = 0;
  const waiting = new Map();
  socket.addEventListener("message", ({ data }) => {
    const message = JSON.parse(data);
    if (message.id === undefined || message.id === null) {
      event(message);
      return;
    }
    const { resolve, reject } = waiting.get(message.id);
    waiting.delete(message.id);
    if (message.error) reject(new Error(message.error.message ?? message.message));
    else resolve(message.result);
  });
  const call = (method, params = {}, extra = {}) => {
    const id = ++sent;
    socket.send(JSON.stringify({ id, method, params, ...extra }));
    return new Promise((resolve, reject) => waiting.set(id, { resolve, reject }));
  };
  return { socket, call };
}

// what the test does to a browser: `navigate(url)`, `evaluate(expression)` for its value,
// `give(paths)` to the file picker, `shot(name, look)`
async function chromium() {
  const { browser, address } = await launch(
    Deno.env.get("CHROMIUM") ?? "chromium",
    ["--headless=new", "--remote-debugging-port=0", `--user-data-dir=${profile}`, "about:blank"],
    /DevTools listening on (ws:\S+)/,
  );
  const { socket, call } = await connect(address, ({ method, params }) => {
    if (method === "Target.attachedToTarget") {
      // a worker: its requests are counted with the page's
      void call("Network.enable", {}, { sessionId: params.sessionId });
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
  const { targetId } = await call("Target.createTarget", { url: "about:blank" });
  const page = (await call("Target.attachToTarget", { targetId, flatten: true })).sessionId;
  const on = (method, params) => call(method, params, { sessionId: page });
  for (const domain of ["Page", "Network", "Log", "Runtime", "Audits"]) await on(`${domain}.enable`);
  await on("Target.setAutoAttach", {
    autoAttach: true,
    waitForDebuggerOnStart: false,
    flatten: true,
  });
  return {
    browser,
    socket,
    close: () => call("Browser.close"),
    navigate: (url) => on("Page.navigate", { url }),
    async evaluate(expression) {
      const { result, exceptionDetails } = await on("Runtime.evaluate", {
        expression,
        returnByValue: true,
        awaitPromise: true,
      });
      check(!exceptionDetails, `${expression}: ${exceptionDetails?.text}`);
      return result.value;
    },
    async give(paths) {
      const { result } = await on("Runtime.evaluate", {
        expression: "document.querySelector('#files')",
      });
      await on("DOM.setFileInputFiles", { objectId: result.objectId, files: paths });
    },
    async shot(name, { width, height, dark }) {
      await on("Emulation.setDeviceMetricsOverride", {
        width,
        height,
        deviceScaleFactor: 2,
        mobile: width < 500,
      });
      await on("Emulation.setEmulatedMedia", {
        features: [{ name: "prefers-color-scheme", value: dark ? "dark" : "light" }],
      });
      const { cssContentSize } = await on("Page.getLayoutMetrics");
      const { data } = await on("Page.captureScreenshot", {
        captureBeyondViewport: true,
        clip: { x: 0, y: 0, width, height: Math.max(height, cssContentSize.height), scale: 1 },
      });
      const bytes = Uint8Array.from(atob(data), (char) => char.charCodeAt(0));
      Deno.writeFileSync(`${shots}/${name}.png`, bytes);
    },
  };
}

async function firefox() {
  const { browser, address } = await launch(
    Deno.env.get("FIREFOX") ?? "firefox",
    ["--headless", "--no-remote", "--remote-debugging-port", "0", "--profile", profile],
    /WebDriver BiDi listening on (ws:\S+)/,
  );
  const { socket, call } = await connect(`${address}/session`, ({ method, params }) => {
    if (method === "network.beforeRequestSent") {
      requests.push(params.request.url);
    } else if (method === "log.entryAdded" && params.level === "error") {
      faults.push(params.text);
    }
  });
  await call("session.new", { capabilities: {} });
  await call("session.subscribe", { events: ["log.entryAdded", "network.beforeRequestSent"] });
  const { contexts } = await call("browsingContext.getTree");
  const context = contexts[0].context;
  const run = async (expression) => {
    const outcome = await call("script.evaluate", {
      expression,
      target: { context },
      awaitPromise: true,
    });
    check(outcome.type === "success", `${expression}: ${outcome.exceptionDetails?.text}`);
    return outcome.result;
  };
  return {
    browser,
    socket,
    close: () => call("browser.close"),
    navigate: (url) => call("browsingContext.navigate", { context, url }),
    async evaluate(expression) {
      // a value comes back as JSON, as the protocol's own form of one is not a plain value
      const { value } = await run(`(async () => JSON.stringify(await (${expression})))()`);
      return value === undefined ? undefined : JSON.parse(value);
    },
    async give(paths) {
      const { sharedId } = await run("document.querySelector('#files')");
      await call("input.setFiles", { context, element: { sharedId }, files: paths });
    },
    // the look of the page is checked in Chromium's screenshots
    async shot() {},
  };
}

check(engine === "chromium" || engine === "firefox", `no browser named ${engine}`);
const driver = await (engine === "chromium" ? chromium() : firefox());
const { browser, socket, evaluate } = driver;

async function open(url) {
  await driver.navigate(url);
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

function give(...names) {
  return driver.give(names.map((name) => `${files}/${name}`));
}

function click(selector) {
  return evaluate(`document.querySelector(${JSON.stringify(selector)}).click()`);
}

async function shot(name, look) {
  if (shots) await driver.shot(name, look);
}

const DESKTOP = { width: 1280, height: 800 };
const PHONE = { width: 390, height: 844 };

async function run() {
  const { origin: plain } = await serve();
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
  check(decoded.text.includes("Format") && decoded.text.includes("FLAC, 44100 Hz"), decoded.text);
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
  await evaluate(`(() => {
    const kept = JSON.parse(localStorage.getItem("choon.readings"));
    kept[0].result.version = 0;
    localStorage.setItem("choon.readings", JSON.stringify(kept));
  })()`);
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

  check(faults.length === 0, `errors or policy violations: ${faults.join("; ")}`);

  // a worker that asks its origin for something as it starts is heard by the server where
  // the policy allows connections, and is not under the page's policy as it is, with no
  // header from the server
  const allowed = await serve({ probe: true, loose: true });
  await open(allowed.origin);
  await until(allowed.probed, "the worker's request comes where the policy allows it");
  const held = await serve({ probe: true });
  await open(held.origin);
  await give("source.mp3");
  await until(async () => (await rows())[0]?.status === "consistent", "a file is read by the worker");
  check(!held.probed(), "the worker's request was not refused");
  // the refusal is logged as the violation it is
  faults.length = 0;

  // under the policy as a header, which binds the worker too, the page reads a file all the same
  const { origin: strict } = await serve({ header: true });
  await open(strict);
  await give("source.mp3");
  await until(async () => (await rows())[0]?.status === "consistent", "a file is read under the header");

  const origins = [plain, strict, allowed.origin, held.origin];
  // the worker's blob is the page's own and the empty icon is a data: URL, and neither is a
  // request to a server
  const elsewhere = requests.filter(
    (url) =>
      url !== "about:blank" &&
      !url.startsWith("data:") &&
      !origins.some((origin) => url.replace(/^blob:/, "").startsWith(`${origin}/`)),
  );
  check(elsewhere.length === 0, `requests left the origin: ${elsewhere.join(", ")}`);
  check(requests.some((url) => WORKER.test(url)), "the worker's requests are counted");
  check(faults.length === 0, `errors or policy violations: ${faults.join("; ")}`);
}

try {
  await run();
} catch (error) {
  console.error(error.message);
  if (faults.length) console.error(`the browser logged: ${faults.join("; ")}`);
  Deno.exitCode = 1;
} finally {
  // a browser told to close ends the processes it started, which write to its profile until
  // they end. one that has not ended after ten seconds is killed. the reply to the close may
  // never come, as the socket closes with the browser
  driver.close().catch(() => {});
  let waited;
  const ended = await Promise.race([
    browser.status.then(() => true),
    new Promise((resolve) => (waited = setTimeout(() => resolve(false), 10_000))),
  ]);
  clearTimeout(waited);
  if (!ended) {
    browser.kill();
    await browser.status;
  }
  socket.close();
  Deno.removeSync(profile, { recursive: true });
  await Promise.all(servers.map((server) => server.shutdown()));
}
