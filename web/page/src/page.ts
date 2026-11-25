// starts the reader in the markup the build rendered for it. Astro's own start of a component
// is a script written into the HTML, which the page's policy refuses
import { hydrate } from "svelte";

import Reader from "./Reader.svelte";

const target = document.querySelector("#reader");
if (target === null) throw new Error("the page has no #reader element");
hydrate(Reader, { target });
