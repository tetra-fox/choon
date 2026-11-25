import svelte from "@astrojs/svelte";
import { defineConfig, envField } from "astro/config";

const text = envField.string({ context: "server", access: "public" });

export default defineConfig({
  integrations: [svelte()],
  outDir: "../dist",
  // the HTML keeps the lines and spaces of its source
  compressHTML: false,
  build: {
    // the page's policy takes no style or script written into the HTML
    inlineStylesheets: "never",
    // links relative to the page, which is then served from any path of a host
    assetsPrefix: ".",
  },
  // what `just web` says of the checkout the page is built from
  env: {
    schema: {
      CHOON_REPOSITORY: text,
      CHOON_COMMIT: text,
      CHOON_COMMIT_SHORT: text,
      CHOON_CHANGED: envField.boolean({ context: "server", access: "public" }),
    },
  },
  vite: {
    // no script or image goes into the file that uses it, where the policy would refuse it
    build: { assetsInlineLimit: 0 },
    // the worker is a module, which the page starts by importing it into a blob
    worker: { format: "es" },
  },
});
