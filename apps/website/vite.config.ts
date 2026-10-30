import { defineConfig } from "vite-plus";
import tailwindcss from "@tailwindcss/vite";

export default defineConfig({
  plugins: [tailwindcss()],
  server: { proxy: { "/api": "http://127.0.0.1:3000" } },
  run: {
    tasks: {
      // Never cached: the result also depends on the Rust server and the browser, and a replayed
      // pass can hide a failure.
      "test:e2e": {
        command: "vp run build && playwright test -c playwright.config.ts",
        cache: false,
      },
    },
  },
});
