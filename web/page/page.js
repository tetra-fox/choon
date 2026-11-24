// the page: takes files, has one worker read them one after another, shows each reading in the
// program's words and keeps the readings in localStorage

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

const picker = document.querySelector("#files");
const choose = document.querySelector("#choose");
const drop = document.querySelector("#drop");
const notice = document.querySelector("#notice");
const section = document.querySelector("#read");
const list = document.querySelector("#readings");
const said = document.querySelector("#said");

// every file shown, top to bottom. an item is `waiting`, `reading`, `read` (with `result`,
// what the worker's `check` returned) or `unread` (with `why`)
let items = [];
// the item the worker is reading
let current = null;
let worker = null;
// the version of the export's JSON, known once a worker is up
let version = null;
let ticking = null;

function element(tag, className, text) {
  const made = document.createElement(tag);
  if (className) made.className = className;
  if (text !== undefined) made.textContent = text;
  return made;
}

// a length of time as the program writes one
function length(seconds) {
  return seconds < 60 ? `${seconds} s` : `${Math.floor(seconds / 60)} min ${seconds % 60} s`;
}

function identity({ name, size, lastModified }) {
  return `${name}\n${size}\n${lastModified}`;
}

function elapsed(item) {
  return length(Math.floor((performance.now() - item.started) / 1000));
}

// fills the item's row for the state it is in
function draw(item) {
  const row = item.row;
  const body = element("div", "body");
  body.append(element("h3", "name", item.name));
  let status;
  if (item.state === "read") {
    const { words, reading } = item.result;
    status = words.verdict;
    row.dataset.verdict = reading.verdict;
    if (words.found.length) body.append(element("p", "found", words.found.join(", ")));
    body.append(element("p", "summary", reading.summary));
    const detail = element("dl");
    const add = (label, values) => {
      detail.append(element("dt", "", label));
      for (const value of values) detail.append(element("dd", "", value));
    };
    add("Format", [words.format]);
    if (words.flags.length) add("Flags", [words.flags.join(", ")]);
    if (reading.limitations.length) add("Limitations", reading.limitations);
    if (item.unkept) add("Kept", ["no, this browser's storage is full or turned off"]);
    body.append(detail);
  } else if (item.state === "unread") {
    status = "not read";
    row.dataset.verdict = "unread";
    body.append(element("p", "found", item.why));
  } else {
    status = item.state;
    delete row.dataset.verdict;
    const line = element("p", "found");
    if (item.state === "reading") line.append(element("span", "time", elapsed(item)));
    const cancel = element("button", "", "Cancel");
    cancel.type = "button";
    cancel.setAttribute("aria-label", `Cancel ${item.name}`);
    cancel.addEventListener("click", () => canceled(item));
    line.append(cancel);
    body.append(line);
  }
  row.replaceChildren(element("p", "status", status), body);
}

function shown() {
  section.hidden = items.length === 0;
}

// writes the readings to localStorage, and says whether it took them
function keep() {
  const kept = items
    .filter((item) => item.state === "read")
    .map(({ name, size, lastModified, result }) => ({ name, size, lastModified, result }));
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
  let kept;
  try {
    kept = JSON.parse(localStorage.getItem(KEPT)) ?? [];
  } catch {
    // storage turned off, or text this page did not write: nothing is remembered
    return;
  }
  const current = kept.filter((entry) => entry.result?.version === version);
  const shownNow = new Set(items.map(identity));
  for (const entry of current) {
    if (shownNow.has(identity(entry))) continue;
    const item = { ...entry, state: "read", row: element("li") };
    draw(item);
    items.push(item);
    list.append(item.row);
  }
  if (current.length !== kept.length) keep();
  shown();
}

function start() {
  // a worker made from a blob is held to the page's policy, connections included. one made
  // from its URL is held to the policy its own response states, and a static host states none
  const source = `import ${JSON.stringify(new URL("worker.js", import.meta.url))};`;
  const blob = URL.createObjectURL(new Blob([source], { type: "text/javascript" }));
  const started = new Worker(blob, { type: "module" });
  URL.revokeObjectURL(blob);
  started.addEventListener("message", ({ data }) => {
    if (data.version !== undefined) {
      const first = version === null;
      version = data.version;
      if (first) restore();
    } else if (data.result !== undefined) {
      finished(current, { result: JSON.parse(data.result) });
    } else {
      finished(current, { why: data.error });
    }
  });
  // a trap, or an analyzer that did not start. its memory is not used again
  started.addEventListener("error", () => {
    started.terminate();
    worker = null;
    if (current) {
      finished(current, {
        why: "the analyzer failed on this file: a bug in choon, or too little memory",
      });
    } else {
      warn("The analyzer did not start in this browser");
    }
  });
  return started;
}

function warn(text) {
  notice.textContent = text;
  notice.hidden = false;
}

function next() {
  if (current) return;
  const item = items.find((item) => item.state === "waiting");
  if (!item) return;
  current = item;
  item.state = "reading";
  item.started = performance.now();
  draw(item);
  ticking = setInterval(() => {
    item.row.querySelector(".time").textContent = elapsed(item);
  }, 1000);
  worker ??= start();
  worker.postMessage(item.file);
}

// `item` ends as read, with `result`, or as not read, with `why`
function finished(item, { result, why }) {
  if (item === current) {
    clearInterval(ticking);
    current = null;
  }
  delete item.file;
  if (result) {
    item.state = "read";
    item.result = result;
    item.unkept = !keep();
    said.textContent = `${item.name}: ${result.words.verdict}`;
  } else {
    item.state = "unread";
    item.why = why;
    said.textContent = `${item.name}: not read (${why})`;
  }
  draw(item);
  next();
}

// a worker inside `check` hears nothing, so the one reading `item` is ended and the next file
// gets another
function canceled(item) {
  if (item === current) {
    worker.terminate();
    worker = null;
  }
  finished(item, { why: "canceled" });
}

function add(files) {
  const batch = [];
  for (const file of files) {
    const same = items.find((item) => identity(item) === identity(file));
    if (same?.file) continue;
    if (same) {
      items = items.filter((item) => item !== same);
      same.row.remove();
    }
    const { name, size, lastModified } = file;
    const item = { name, size, lastModified, row: element("li") };
    if (size > MAX_FILE_BYTES) {
      item.state = "unread";
      item.why = `larger than ${MAX_FILE_WORDS}`;
    } else {
      item.state = "waiting";
      item.file = file;
    }
    draw(item);
    batch.push(item);
  }
  items = [...batch, ...items];
  list.prepend(...batch.map((item) => item.row));
  shown();
  next();
}

document.querySelector("#limit").textContent = MAX_FILE_WORDS;

if (WebAssembly.validate(SIMD)) {
  // the worker says the export's version, which the kept readings are shown by
  worker = start();
  choose.addEventListener("click", () => picker.click());
  picker.addEventListener("change", () => {
    add(picker.files);
    picker.value = "";
  });
  // a file dropped anywhere on the page is taken, as the browser would open it in the tab
  document.addEventListener("dragover", (event) => {
    if (!event.dataTransfer.types.includes("Files")) return;
    event.preventDefault();
    drop.classList.add("over");
  });
  document.addEventListener("dragleave", (event) => {
    if (event.relatedTarget === null) drop.classList.remove("over");
  });
  document.addEventListener("drop", (event) => {
    if (!event.dataTransfer.types.includes("Files")) return;
    event.preventDefault();
    drop.classList.remove("over");
    add(event.dataTransfer.files);
  });
  document.querySelector("#clear").addEventListener("click", () => {
    for (const item of items) if (!item.file) item.row.remove();
    items = items.filter((item) => item.file);
    try {
      localStorage.removeItem(KEPT);
    } catch {
      // storage turned off: nothing was kept
    }
    shown();
  });
} else {
  choose.disabled = true;
  warn("This browser lacks WebAssembly SIMD, which the analyzer needs. No file can be read here");
}
