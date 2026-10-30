import { expect, test, type Browser, type Page } from "@playwright/test";
import { createHash } from "node:crypto";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createServer, request as forward } from "node:http";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";

// The server stores uploads in 16 MiB parts, so this file is sent as three parts.
const SIZE = 40 * 1024 * 1024;
let directory = "";

test.beforeAll(async () => {
  directory = await mkdtemp(join(tmpdir(), "hokan-e2e-"));
});
test.afterAll(async () => {
  await rm(directory, { recursive: true, force: true });
});
test.setTimeout(120_000);

async function makeFile(name: string) {
  const contents = Buffer.alloc(SIZE);
  for (let offset = 0; offset < SIZE; offset += 4) contents.writeUInt32BE(offset, offset);
  const path = join(directory, name);
  await writeFile(path, contents);
  return { path, name, sha256: createHash("sha256").update(contents).digest("hex") };
}

async function shareFile(page: Page, path: string) {
  await page.locator("#file-input").setInputFiles(path);
  await page.getByRole("button", { name: "Create a private link" }).click();
  const link = page.getByLabel("Private share link");
  await expect(link).toBeVisible({ timeout: 90_000 });
  return link.inputValue();
}

async function downloadHash(browser: Browser, link: string, name: string) {
  const recipient = await browser.newContext({ acceptDownloads: true });
  try {
    const page = await recipient.newPage();
    await page.goto(link);
    await expect(page.getByText(`Ready to download: ${name}`)).toBeVisible({ timeout: 60_000 });
    const download = page.waitForEvent("download");
    await page.getByRole("link", { name: "Download your file" }).click();
    const contents = await readFile((await (await download).path())!);
    return createHash("sha256").update(contents).digest("hex");
  } finally {
    await recipient.close();
  }
}

const partOf = (url: string) => Number(url.split("/").at(-1));

// Forwards requests to the app, but drops the connection of the first file download after
// `cut` bytes. Returns the Range header of each file download it forwards.
async function cuttingProxy(target: URL, cut: number) {
  const ranges: Array<string | undefined> = [];
  const proxy = createServer((incoming, outgoing) => {
    const upstream = forward(
      {
        host: target.hostname,
        port: target.port,
        path: incoming.url,
        method: incoming.method,
        headers: incoming.headers,
      },
      (response) => {
        outgoing.writeHead(response.statusCode!, response.headers);
        if (!incoming.url!.startsWith("/api/files/")) return response.pipe(outgoing);
        ranges.push(incoming.headers.range);
        if (ranges.length > 1) return response.pipe(outgoing);
        let sent = 0;
        response.on("data", (chunk: Buffer) => {
          if (sent >= cut) return;
          const piece = chunk.subarray(0, cut - sent);
          sent += piece.length;
          outgoing.write(piece);
          if (sent < cut) return;
          response.destroy();
          // Ending the socket sends what was written, then closes the connection mid-body.
          outgoing.socket?.end();
        });
      },
    );
    incoming.pipe(upstream);
  });
  await new Promise<void>((resolve) => proxy.listen(0, "127.0.0.1", resolve));
  const { port } = proxy.address() as AddressInfo;
  return {
    origin: `http://127.0.0.1:${port}`,
    ranges,
    close() {
      proxy.closeAllConnections();
      return new Promise((resolve) => proxy.close(resolve));
    },
  };
}

test("continues a download from where the connection dropped", async ({ browser, page }) => {
  const file = await makeFile("download.bin");
  await page.goto("/");
  const link = new URL(await shareFile(page, file.path));
  const cut = 10 * 1024 * 1024;
  const proxy = await cuttingProxy(link, cut);
  try {
    const proxied = `${proxy.origin}${link.pathname}${link.hash}`;
    expect(await downloadHash(browser, proxied, file.name)).toBe(file.sha256);
    expect(proxy.ranges).toHaveLength(2);
    expect(proxy.ranges[0]).toBeUndefined();
    const resumedAt = Number(/^bytes=(\d+)-$/.exec(proxy.ranges[1] ?? "")?.[1]);
    expect(resumedAt).toBeGreaterThan(0);
    expect(resumedAt).toBeLessThanOrEqual(cut);
  } finally {
    await proxy.close();
  }
});

test("keeps uploading through dropped connections and server errors", async ({ browser, page }) => {
  const file = await makeFile("flaky.bin");
  const attempts = new Map<number, number>();
  await page.route("**/api/uploads/*/parts/*", (route) => {
    const part = partOf(route.request().url());
    const attempt = (attempts.get(part) ?? 0) + 1;
    attempts.set(part, attempt);
    if (part === 2 && attempt === 1) return route.abort("connectionreset");
    if (part === 3 && attempt === 1)
      return route.fulfill({ status: 503, headers: { "Retry-After": "1" }, body: "Busy" });
    return route.continue();
  });
  await page.goto("/");
  const link = await shareFile(page, file.path);
  expect(Object.fromEntries(attempts)).toEqual({ 1: 1, 2: 2, 3: 2 });
  expect(await downloadHash(browser, link, file.name)).toBe(file.sha256);
});

test("waits while the browser is offline and continues afterwards", async ({
  browser,
  context,
  page,
}) => {
  const file = await makeFile("offline.bin");
  await page.goto("/");
  let offline = false;
  page.on("request", (request) => {
    if (!offline && request.method() === "PUT") {
      offline = true;
      void context.setOffline(true);
    }
  });
  await page.locator("#file-input").setInputFiles(file.path);
  await page.getByRole("button", { name: "Create a private link" }).click();
  await expect(page.getByText(/You are offline|Connection problem/)).toBeVisible();
  await expect(page.getByRole("button", { name: "Reconnecting…" })).toBeVisible();
  await context.setOffline(false);
  const link = page.getByLabel("Private share link");
  await expect(link).toBeVisible({ timeout: 90_000 });
  expect(await downloadHash(browser, await link.inputValue(), file.name)).toBe(file.sha256);
});

test("resumes after the page is reloaded without sending finished parts again", async ({
  browser,
  page,
}) => {
  const file = await makeFile("resume.bin");
  const sent = new Map<number, number>();
  let hold = true;
  await page.route("**/api/uploads/*/parts/*", (route) => {
    const part = partOf(route.request().url());
    // Leave the later parts hanging, as on a connection that drops before the page closes.
    if (hold && part !== 1) return;
    sent.set(part, (sent.get(part) ?? 0) + 1);
    return route.continue();
  });
  let sessions = 0;
  page.on("request", (request) => {
    if (request.method() === "POST" && request.url().endsWith("/api/uploads")) sessions++;
  });
  const warnings: string[] = [];
  page.on("dialog", (dialog) => {
    warnings.push(dialog.type());
    void dialog.accept();
  });
  await page.goto("/");
  await page.locator("#file-input").setInputFiles(file.path);
  const firstPart = page.waitForResponse(
    (response) => response.url().endsWith("/parts/1") && response.status() === 204,
  );
  await page.getByRole("button", { name: "Create a private link" }).click();
  await firstPart;
  await page.reload();
  expect(warnings).toEqual(["beforeunload"]);
  hold = false;
  await expect(
    page.getByText(/Your upload of “resume.bin” \(40.0 MiB\) was interrupted/),
  ).toBeVisible();
  const link = await shareFile(page, file.path);
  expect(sessions).toBe(1);
  expect(Object.fromEntries(sent)).toEqual({ 1: 1, 2: 1, 3: 1 });
  await expect(page.getByText(/was interrupted/)).toBeHidden();
  expect(await downloadHash(browser, link, file.name)).toBe(file.sha256);
});

test("starts again when the server no longer has the upload", async ({ browser, page }) => {
  const file = await makeFile("restart.bin");
  let lost = false;
  await page.route("**/api/uploads/*/parts/*", (route) => {
    if (!lost && partOf(route.request().url()) === 2) {
      lost = true;
      return route.fulfill({ status: 404, contentType: "text/plain", body: "Upload not found" });
    }
    return route.continue();
  });
  let sessions = 0;
  page.on("request", (request) => {
    if (request.method() === "POST" && request.url().endsWith("/api/uploads")) sessions++;
  });
  await page.goto("/");
  const link = await shareFile(page, file.path);
  expect(sessions).toBe(2);
  expect(await downloadHash(browser, link, file.name)).toBe(file.sha256);
});

test("cancelling an upload tells the server and keeps nothing to resume", async ({ page }) => {
  const file = await makeFile("cancel.bin");
  await page.route("**/api/uploads/*/parts/*", () => undefined);
  await page.goto("/");
  await page.locator("#file-input").setInputFiles(file.path);
  const part = page.waitForRequest((request) => request.method() === "PUT");
  await page.getByRole("button", { name: "Create a private link" }).click();
  await part;
  const cancelled = page.waitForRequest((request) => request.method() === "DELETE");
  await page.getByRole("button", { name: "Cancel upload" }).click();
  await cancelled;
  await expect(page.getByText("Upload cancelled.")).toBeVisible();
  await expect(page.getByRole("button", { name: "Create a private link" })).toBeEnabled();
  await page.reload();
  await expect(page.getByRole("heading", { name: "Share a file" })).toBeVisible();
  await expect(page.getByText(/was interrupted/)).toBeHidden();
});
