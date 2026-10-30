import { defineConfig } from "vite-plus";
import tailwindcss from "@tailwindcss/vite";

export default defineConfig({
  plugins: [tailwindcss()],
  server: { proxy: { "/api": "http://127.0.0.1:3000" } },
});
