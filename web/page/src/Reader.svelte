<script lang="ts">
  // takes files, has one worker read them one after another, shows each reading in the
  // program's words and keeps the readings in localStorage
  import { onMount } from "svelte";

  import type { Posted } from "./worker.ts";
  import workerUrl from "./worker.ts?worker&url";

  // what `check` returns, as far as the page shows it: web/README.md has the whole shape
  type Result = {
    version: number;
    words: { verdict: string; found: string[]; format: string; flags: string[] };
    reading: { verdict: string; summary: string; limitations: string[] };
  };

  // what a file is known by, in the list and in localStorage
  type Named = { name: string; size: number; lastModified: number };

  type Kept = Named & { result: Result };

  // a file shown. `started` is the time its reading began, `result` what the worker's `check`
  // returned, `unkept` that localStorage refused it, `why` the reason it was not read
  type Item = Named & { id: number } & (
      | { state: "waiting"; file: File }
      | { state: "reading"; file: File; started: number }
      | { state: "read"; result: Result; unkept: boolean }
      | { state: "unread"; why: string }
    );

  // the largest file read, in bytes: web/README.md has the reasoning
  const MAX_FILE_BYTES = 256 * 1024 * 1024;
  const MAX_FILE_WORDS = `${MAX_FILE_BYTES / 1024 / 1024} MiB`;

  // the localStorage key of the readings kept
  const KEPT = "choon.readings";

  // a module whose one function uses a 128-bit vector, which validates only with wasm SIMD
  const SIMD = new Uint8Array([
    0, 97, 115, 109, 1, 0, 0, 0, 1, 5, 1, 96, 0, 1, 123, 3, 2, 1, 0, 10, 10, 1, 8, 0, 65, 0, 253,
    15, 253, 98, 11,
  ]);

  // every file shown, top to bottom
  let items = $state.raw<Item[]>([]);
  let notice = $state("");
  // the last file finished, for a screen reader
  let said = $state("");
  let over = $state(false);
  // whether files are taken, which they are not without wasm SIMD
  let taking = $state(true);
  // the time the elapsed times are shown at
  let now = $state(0);
  let picker: HTMLInputElement;

  let made = 0;
  // the id of the item the worker is reading
  let current: number | null = null;
  let worker: Worker | null = null;
  // the version of the export's JSON, known once a worker is up
  let version: number | null = null;
  let ticking: ReturnType<typeof setInterval> | undefined;

  // a length of time in whole seconds, in the program's words from 10 s on
  function length(seconds: number) {
    return seconds < 60 ? `${seconds} s` : `${Math.floor(seconds / 60)} min ${seconds % 60} s`;
  }

  function identity({ name, size, lastModified }: Named) {
    return `${name}\n${size}\n${lastModified}`;
  }

  function put(item: Item) {
    items = items.map((old) => (old.id === item.id ? item : old));
  }

  // writes the readings to localStorage, and says whether it took them
  function keep() {
    const kept: Kept[] = items.flatMap((item) => {
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
    let kept: Kept[];
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
      .map((entry): Item => ({ ...entry, id: made++, state: "read", unkept: false }));
    items = [...items, ...restored];
    if (same.length !== kept.length) keep();
  }

  function start() {
    // a worker made from a blob is held to the page's policy, connections included. one made
    // from its URL is held to the policy its own response states, and a static host states none.
    // the worker's URL is relative to the page
    const source = `import ${JSON.stringify(new URL(workerUrl, document.baseURI))};`;
    const blob = URL.createObjectURL(new Blob([source], { type: "text/javascript" }));
    const started = new Worker(blob, { type: "module" });
    URL.revokeObjectURL(blob);
    started.addEventListener("message", ({ data }: MessageEvent<Posted>) => {
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
        notice = "The analyzer did not start in this browser";
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
    if (current !== null) return;
    const item = items.find((item) => item.state === "waiting");
    if (!item) return;
    current = item.id;
    now = performance.now();
    put({ ...item, state: "reading", started: now });
    ticking = setInterval(() => (now = performance.now()), 1000);
    worker ??= start();
    worker.postMessage(item.file);
  }

  // the item of `id` ends as read, with `result`, or as not read, with `why`
  function finished(id: number, end: { result: Result } | { why: string }) {
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
      said = `${name}: ${end.result.words.verdict}`;
    } else {
      put({ ...named, state: "unread", why: end.why });
      said = `${name}: not read (${end.why})`;
    }
    next();
  }

  // a worker inside `check` hears nothing, so the one reading the item is ended and the next
  // file gets another
  function canceled(id: number) {
    if (id === current) {
      worker?.terminate();
      worker = null;
    }
    finished(id, { why: "canceled" });
  }

  function add(files: FileList) {
    const batch: Item[] = [];
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
  }

  // a file dropped anywhere on the page is taken, as the browser would open it in the tab
  function dragged(event: DragEvent) {
    if (!taking || !event.dataTransfer?.types.includes("Files")) return;
    event.preventDefault();
    over = true;
  }

  function left(event: DragEvent) {
    if (event.relatedTarget === null) over = false;
  }

  function dropped(event: DragEvent) {
    if (!taking || !event.dataTransfer?.types.includes("Files")) return;
    event.preventDefault();
    over = false;
    add(event.dataTransfer.files);
  }

  onMount(() => {
    if (WebAssembly.validate(SIMD)) {
      // the worker says the export's version, which the kept readings are shown by
      worker = start();
    } else {
      taking = false;
      notice =
        "This browser lacks WebAssembly SIMD, which the analyzer needs to read a file";
    }
  });
</script>

<svelte:document ondragover={dragged} ondragleave={left} ondrop={dropped} />

<section id="drop" class={{ over }} aria-labelledby="drop-title">
  <h2 id="drop-title">Drop audio files here</h2>
  <button id="choose" type="button" disabled={!taking} onclick={() => picker.click()}>
    Choose files
  </button>
  <input
    id="files"
    type="file"
    multiple
    hidden
    bind:this={picker}
    onchange={() => {
      if (picker.files) add(picker.files);
      picker.value = "";
    }}
  />
  <p id="notice" role="alert" hidden={!notice}>{notice}</p>
  <p class="hint">
    A file in a lossless format takes from several seconds to around a minute, depending on the
    device. Other files take far less. Files over <span id="limit">{MAX_FILE_WORDS}</span> are not
    read.
  </p>
</section>

<section id="read" aria-labelledby="read-title" hidden={items.length === 0}>
  <div class="heading">
    <h2 id="read-title">Readings</h2>
    <button id="clear" type="button" onclick={clear}>Clear</button>
  </div>
  <p class="hint">Kept in this browser until cleared</p>
  <ul id="readings">
    {#each items as item (item.id)}
      {#if item.state === "read"}
        {@const { words, reading } = item.result}
        <li data-verdict={reading.verdict}>
          <p class="status">{words.verdict}</p>
          <div class="body">
            <h3 class="name">{item.name}</h3>
            {#if words.found.length}<p class="found">{words.found.join(", ")}</p>{/if}
            <p class="summary">{reading.summary}</p>
            <!-- a label and its value have no space between them, so a row's text has none -->
            <dl>
              <dt>Format</dt><dd>{words.format}</dd>
              {#if words.flags.length}<dt>Flags</dt><dd>{words.flags.join(", ")}</dd>{/if}
              {#if reading.limitations.length}
                <dt>Limitations</dt>{#each reading.limitations as limitation}<dd>{limitation}</dd>{/each}
              {/if}
              {#if item.unkept}
                <dt>Kept</dt><dd>no, this browser's storage is full or turned off</dd>
              {/if}
            </dl>
          </div>
        </li>
      {:else if item.state === "unread"}
        <li data-verdict="unread">
          <p class="status">not read</p>
          <div class="body">
            <h3 class="name">{item.name}</h3>
            <p class="found">{item.why}</p>
          </div>
        </li>
      {:else}
        <li>
          <p class="status">{item.state}</p>
          <div class="body">
            <h3 class="name">{item.name}</h3>
            <p class="found">
              {#if item.state === "reading"}
                <span class="time">{length(Math.floor((now - item.started) / 1000))}</span>
              {/if}
              <button type="button" aria-label={`Cancel ${item.name}`} onclick={() => canceled(item.id)}>
                Cancel
              </button>
            </p>
          </div>
        </li>
      {/if}
    {/each}
  </ul>
  <p id="said" class="unseen" role="status">{said}</p>
</section>
