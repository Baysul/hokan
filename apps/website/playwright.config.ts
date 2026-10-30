import { defineConfig } from "@playwright/test";
import { tmpdir } from "node:os";
import { join } from "node:path";

export default defineConfig({
  testDir: "./e2e",
  testMatch: "**/*.e2e.ts",
  fullyParallel: false,
  workers: 1,
  reporter: "list",
  use: {
    baseURL: "http://127.0.0.1:3017",
    browserName: "firefox",
    headless: true,
    acceptDownloads: true,
  },
  webServer: {
    command: "cargo run --manifest-path ../server/Cargo.toml",
    url: "http://127.0.0.1:3017/health",
    timeout: 180_000,
    reuseExistingServer: false,
    env: {
      HOKAN_ADDRESS: "127.0.0.1:3017",
      CARGO_TARGET_DIR: join(tmpdir(), "hokan-playwright-target"),
    },
  },
});
