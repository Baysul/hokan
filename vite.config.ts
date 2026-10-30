import { defineConfig } from "vite-plus";

export default defineConfig({
  staged: {
    "*.{js,jsx,mjs,cjs,ts,tsx,mts,cts}": "vp check --fix",
    "*.{css,html,json,jsonc,md,mdx,yaml,yml}": "vp fmt",
  },
  fmt: {},
  lint: {
    jsPlugins: [{ name: "vite-plus", specifier: "vite-plus/oxlint-plugin" }],
    rules: { "vite-plus/prefer-vite-plus-imports": "error" },
    options: { typeAware: true, typeCheck: true },
  },
  run: {
    cache: true,
  },
});
