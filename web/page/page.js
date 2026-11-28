// takes files, has one worker read them one after another, shows each reading in the
// program's words and keeps the readings in localStorage

/** @typedef {import("./worker.js").Posted} Posted */

/**
 * what `check` returns, as far as the page shows it: web/README.md has the whole shape
 * @typedef {{
 *   version: number,
 *   words: { verdict: string, found: string[], format: string, flags: string[] },
 *   reading: { verdict: string, summary: string, limitations: string[] },
 * }} Result
 */

/**
 * what a file is known by, in the list and in localStorage
 * @typedef {{ name: string, size: number, lastModified: number }} Named
 */

/** @typedef {Named & { result: Result }} Kept */

/**
 * a file shown. `started` is the time its reading began, `result` what the worker's `check`
 * returned, `unkept` that localStorage refused it, `why` the reason it was not read
 * @typedef {Named & { id: number } & (
 *   | { state: "waiting", file: File }
 *   | { state: "reading", file: File, started: number }
 *   | { state: "read", result: Result, unkept: boolean }
 *   | { state: "unread", why: string }
 * )} Item
 */

// the largest file read, in bytes: web/README.md has the reasoning
const MAX_FILE_BYTES = 256 * 1024 * 1024;
const MAX_FILE_WORDS = `${MAX_FILE_BYTES / 1024 / 1024} MiB`;

// the localStorage key of the readings kept
const KEPT = "choon.readings";

// a module whose one function uses a 128-bit vector, which validates only with wasm SIMD
const SIMD = new Uint8Array([
  0, 97, 115, 109, 1, 0, 0, 0, 1, 5, 1, 96, 0, 1, 123, 3, 2, 1, 0, 10, 10, 1, 8, 0, 65, 0, 253, 15,
  253, 98, 11,
]);

/**
 * the element of `selector`, which index.html has
 * @template {HTMLElement} T
 * @param {string} selector
 * @param {new () => T} type
 */
function element(selector, type) {
  const found = document.querySelector(selector);
  if (!(found instanceof type)) throw new Error(`the page has no ${selector}`);
  return found;
}

const drop = element("#drop", HTMLElement);
const choose = element("#choose", HTMLButtonElement);
const picker = element("#files", HTMLInputElement);
const notice = element("#notice", HTMLParagraphElement);
const read = element("#read", HTMLElement);
const list = element("#readings", HTMLUListElement);
// the last file finished, for a screen reader
const said = element("#said", HTMLParagraphElement);

/** every file shown, top to bottom @type {Item[]} */
let items = [];
// the row shown for each item, by id, with the item it was made from
/** @type {Map<number, { item: Item, row: HTMLLIElement }>} */
const rows = new Map();
// whether files are taken, which they are not without wasm SIMD
let taking = true;
let made = 0;
// the id of the item the worker is reading
/** @type {number | null} */
let current = null;
/** @type {Worker | null} */
let worker = null;
// the version of the export's JSON, known once a worker is up
/** @type {number | null} */
let version = null;
/** @type {ReturnType<typeof setInterval> | undefined} */
let ticking;

/**
 * a length of time in whole seconds, in the program's words from 10 s on
 * @param {number} seconds
 */
function length(seconds) {
  return seconds < 60 ? `${seconds} s` : `${Math.floor(seconds / 60)} min ${seconds % 60} s`;
}

/** @param {Named} named */
function identity({ name, size, lastModified }) {
  return `${name}\n${size}\n${lastModified}`;
}

/**
 * an element of `tag` with `text`, or with `children`
 * @param {string} tag
 * @param {string} className
 * @param {string | Node[]} content
 */
function make(tag, className, content) {
  const made = document.createElement(tag);
  if (className) made.className = className;
  if (typeof content === "string") made.textContent = content;
  else made.append(...content);
  return made;
}

/**
 * the row of `item`, laid out as the program's line: the status, then the file and what was
 * found. a label and its value have no space between them, so a row's text has none
 * @param {Item} item
 */
function row(item) {
  const li = document.createElement("li");
  const name = make("h3", "name", item.name);
  if (item.state === "read") {
    const { words, reading } = item.result;
    li.dataset.verdict = reading.verdict;
    const details = [make("dt", "", "Format"), make("dd", "", words.format)];
    if (words.flags.length) details.push(make("dt", "", "Flags"), make("dd", "", words.flags.join(", ")));
    if (reading.limitations.length) {
      details.push(make("dt", "", "Limitations"));
      for (const limitation of reading.limitations) details.push(make("dd", "", limitation));
    }
    if (item.unkept) {
      details.push(make("dt", "", "Kept"), make("dd", "", "no, this browser's storage is full or turned off"));
    }
    const body = [name];
    if (words.found.length) body.push(make("p", "found", words.found.join(", ")));
    body.push(make("p", "summary", reading.summary), make("dl", "", details));
    li.append(make("p", "status", words.verdict), make("div", "body", body));
  } else if (item.state === "unread") {
    li.dataset.verdict = "unread";
    li.append(make("p", "status", "not read"), make("div", "body", [name, make("p", "found", item.why)]));
  } else {
    const cancel = make("button", "", "Cancel");
    cancel.setAttribute("type", "button");
    cancel.setAttribute("aria-label", `Cancel ${item.name}`);
    cancel.addEventListener("click", () => canceled(item.id));
    const found = item.state === "reading" ? [make("span", "time", ""), cancel] : [cancel];
    li.append(make("p", "status", item.state), make("div", "body", [name, make("p", "found", found)]));
  }
  return li;
}

// shows `items`. a row is made again only when its item changed, so a focused Cancel button
// keeps its focus while the times tick
function render() {
  read.hidden = items.length === 0;
  const shown = items.map((item) => {
    const old = rows.get(item.id);
    if (old?.item === item) return old.row;
    const made = { item, row: row(item) };
    rows.set(item.id, made);
    return made.row;
  });
  for (const id of rows.keys()) {
    if (!items.some((item) => item.id === id)) rows.delete(id);
  }
  if (shown.length !== list.children.length || shown.some((row, at) => list.children[at] !== row)) {
    list.replaceChildren(...shown);
  }
  const now = performance.now();
  for (const item of items) {
    if (item.state !== "reading") continue;
    const time = rows.get(item.id)?.row.querySelector(".time");
    if (time) time.textContent = length(Math.floor((now - item.started) / 1000));
  }
}

/** @param {Item} item */
function put(item) {
  items = items.map((old) => (old.id === item.id ? item : old));
}

// writes the readings to localStorage, and says whether it took them
function keep() {
  /** @type {Kept[]} */
  const kept = items.flatMap((item) => {
    if (item.state !== "read") return [];
    const { name, size, lastModified, result } = item;
    return [{ name, size, lastModified, result }];
  });
  try {
    localStorage.setItem(KEPT, JSON.stringify(kept));
    return true;
  } catch {
    // full, or turned off as in some private windows. the reading says it was not kept
    return false;
  }
}

// the readings kept by a page whose export has this version, under the items already shown
function restore() {
  /** @type {Kept[]} */
  let kept;
  try {
    kept = JSON.parse(localStorage.getItem(KEPT) ?? "[]");
  } catch {
    // storage turned off, or text this page did not write: nothing is remembered
    return;
  }
  const same = kept.filter((entry) => entry.result?.version === version);
  const shown = new Set(items.map(identity));
  const restored = same
    .filter((entry) => !shown.has(identity(entry)))
    .map((entry) => /** @type {Item} */ ({ ...entry, id: made++, state: "read", unkept: false }));
  items = [...items, ...restored];
  if (same.length !== kept.length) keep();
  render();
}

function start() {
  // a worker made from a blob is held to the page's policy: web/README.md has the reasoning
  const source = `import ${JSON.stringify(new URL("worker.js", import.meta.url))};`;
  const blob = URL.createObjectURL(new Blob([source], { type: "text/javascript" }));
  const started = new Worker(blob, { type: "module" });
  URL.revokeObjectURL(blob);
  started.addEventListener("message", (/** @type {MessageEvent<Posted>} */ { data }) => {
    if ("version" in data) {
      const first = version === null;
      version = data.version;
      if (first) restore();
    } else if ("result" in data) {
      finished(posted(), { result: JSON.parse(data.result) });
    } else {
      finished(posted(), { why: data.error });
    }
  });
  // a trap, or an analyzer that did not start. its memory is not used again
  started.addEventListener("error", () => {
    started.terminate();
    worker = null;
    if (current === null) {
      notice.textContent = "The analyzer did not start in this browser";
      notice.hidden = false;
    } else {
      finished(current, {
        why: "the analyzer failed on this file: a bug in choon, or too little memory",
      });
    }
  });
  return started;
}

// the id of the file the worker answers, which is the one posted to it
function posted() {
  if (current === null) throw new Error("the worker answered with no file posted to it");
  return current;
}

function next() {
  render();
  if (current !== null) return;
  const item = items.find((item) => item.state === "waiting");
  if (!item || !("file" in item)) return;
  current = item.id;
  put({ ...item, state: "reading", started: performance.now() });
  ticking = setInterval(render, 1000);
  worker ??= start();
  worker.postMessage(item.file);
  render();
}

/**
 * the item of `id` ends as read, with `result`, or as not read, with `why`
 * @param {number} id
 * @param {{ result: Result } | { why: string }} end
 */
function finished(id, end) {
  if (id === current) {
    clearInterval(ticking);
    current = null;
  }
  const item = items.find((item) => item.id === id);
  if (!item) throw new Error("the file that ended is not in the list");
  const { name, size, lastModified } = item;
  const named = { id, name, size, lastModified };
  if ("result" in end) {
    put({ ...named, state: "read", result: end.result, unkept: false });
    if (!keep()) put({ ...named, state: "read", result: end.result, unkept: true });
    said.textContent = `${name}: ${end.result.words.verdict}`;
  } else {
    put({ ...named, state: "unread", why: end.why });
    said.textContent = `${name}: not read (${end.why})`;
  }
  next();
}

// a worker inside `check` hears nothing, so the one reading the item is ended and the next
// file gets another
/** @param {number} id */
function canceled(id) {
  if (id === current) {
    worker?.terminate();
    worker = null;
  }
  finished(id, { why: "canceled" });
}

/** @param {FileList} files */
function add(files) {
  /** @type {Item[]} */
  const batch = [];
  for (const file of files) {
    const same = items.find((item) => identity(item) === identity(file));
    if (same && "file" in same) continue;
    if (same) items = items.filter((item) => item !== same);
    const { name, size, lastModified } = file;
    const named = { id: made++, name, size, lastModified };
    batch.push(
      size > MAX_FILE_BYTES
        ? { ...named, state: "unread", why: `larger than ${MAX_FILE_WORDS}` }
        : { ...named, state: "waiting", file },
    );
  }
  items = [...batch, ...items];
  next();
}

function clear() {
  items = items.filter((item) => "file" in item);
  try {
    localStorage.removeItem(KEPT);
  } catch {
    // storage turned off: nothing was kept
  }
  render();
}

/** @param {DragEvent} event */
function dragged(event) {
  if (!taking || !event.dataTransfer?.types.includes("Files")) return;
  event.preventDefault();
  drop.classList.add("over");
}

// a file dropped anywhere on the page is taken, as the browser would open it in the tab
document.addEventListener("dragover", dragged);
document.addEventListener("dragleave", (event) => {
  if (event.relatedTarget === null) drop.classList.remove("over");
});
document.addEventListener("drop", (event) => {
  if (!taking || !event.dataTransfer?.types.includes("Files")) return;
  event.preventDefault();
  drop.classList.remove("over");
  add(event.dataTransfer.files);
});
choose.addEventListener("click", () => picker.click());
picker.addEventListener("change", () => {
  if (picker.files) add(picker.files);
  picker.value = "";
});
element("#clear", HTMLButtonElement).addEventListener("click", clear);
element("#limit", HTMLSpanElement).textContent = MAX_FILE_WORDS;

if (WebAssembly.validate(SIMD)) {
  // the worker says the export's version, which the kept readings are shown by
  worker = start();
} else {
  taking = false;
  choose.disabled = true;
  notice.textContent = "The analyzer needs WebAssembly SIMD, which this browser lacks";
  notice.hidden = false;
}
