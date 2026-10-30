import { expect, test } from "@playwright/test";
import { readFile } from "node:fs/promises";

test("encrypts a file, shares a link and downloads it in Firefox", async ({ browser, page }) => {
  const contents = Buffer.from("a secret message for Firefox ✨", "utf8");
  const parts: Buffer[] = [];
  let expiry: string | undefined;
  page.on("request", (request) => {
    if (request.method() === "POST" && request.url().endsWith("/api/uploads"))
      expiry = request.headers()["x-hokan-expiry-seconds"];
    const body = request.method() === "PUT" && request.postDataBuffer();
    if (body) parts.push(body);
  });
  const homeResponse = await page.goto("/");
  expect(homeResponse?.headers()["content-security-policy"]).toContain("script-src 'self'");
  expect(homeResponse?.headers()["referrer-policy"]).toBe("no-referrer");
  await expect(page.getByRole("heading", { name: "Share a file" })).toBeVisible();
  await page
    .locator("#file-input")
    .setInputFiles({ name: "secret.txt", mimeType: "text/plain", buffer: contents });
  await page.getByLabel("Link expires").selectOption("3600");
  await page.getByRole("button", { name: "Create a private link" }).click();
  const linkInput = page.getByLabel("Private share link");
  await expect(linkInput).toBeVisible();
  const shareLink = await linkInput.inputValue();
  expect(shareLink).toMatch(/\/share\/[0-9a-f-]{36}#[A-Za-z0-9_-]{43}$/);
  expect(parts).toHaveLength(1);
  expect(expiry).toBe("3600");
  expect(parts[0]!.includes(contents)).toBe(false);
  expect(parts[0]!.includes(Buffer.from("secret.txt"))).toBe(false);

  // A separate browser context models the recipient without sharing sender state.
  const recipient = await browser.newContext({ acceptDownloads: true });
  try {
    const recipientPage = await recipient.newPage();
    const shareResponse = await recipientPage.goto(shareLink);
    expect(shareResponse?.headers()["cache-control"]).toBe("no-store");
    await expect(recipientPage.getByText("Ready to download: secret.txt")).toBeVisible();
    const downloadPromise = recipientPage.waitForEvent("download");
    await recipientPage.getByRole("link", { name: "Download your file" }).click();
    const download = await downloadPromise;
    expect(download.suggestedFilename()).toBe("secret.txt");
    expect(await readFile((await download.path())!)).toEqual(contents);
    const [url, key] = shareLink.split("#");
    const changedKey = `${key![0] === "A" ? "B" : "A"}${key!.slice(1)}`;
    await recipientPage.goto(`${url}#${changedKey}`);
    await expect(recipientPage.getByText(/Could not decrypt this file/)).toBeVisible();
    await recipientPage.goto(shareLink);
    await expect(recipientPage.getByText("Ready to download: secret.txt")).toBeVisible();
  } finally {
    await recipient.close();
  }
});

test("creates a link that never expires and says so", async ({ page }) => {
  let expiry: string | undefined;
  page.on("request", (request) => {
    if (request.method() === "POST" && request.url().endsWith("/api/uploads"))
      expiry = request.headers()["x-hokan-expiry-seconds"];
  });
  await page.goto("/");
  await page
    .locator("#file-input")
    .setInputFiles({ name: "keep.txt", mimeType: "text/plain", buffer: Buffer.from("keep me") });
  await page.getByLabel("Link expires").selectOption({ label: "Never" });
  await page.getByRole("button", { name: "Create a private link" }).click();
  await expect(page.getByLabel("Private share link")).toBeVisible();
  await expect(page.getByText("It never expires.")).toBeVisible();
  expect(expiry).toBe("0");
});

test("explains why an upload failed instead of a generic error", async ({ page }) => {
  await page.route("**/api/uploads", (route) =>
    route.fulfill({ status: 507, contentType: "text/plain", body: "Storage capacity reached" }),
  );
  await page.goto("/");
  await page
    .locator("#file-input")
    .setInputFiles({ name: "full.txt", mimeType: "text/plain", buffer: Buffer.from("hello") });
  await page.getByRole("button", { name: "Create a private link" }).click();
  await expect(
    page.getByText("The server could not store the file (Storage capacity reached)."),
  ).toBeVisible();
  // The form is usable again so the sender can retry.
  await expect(page.getByRole("button", { name: "Create a private link" })).toBeEnabled();
  await expect(page.getByLabel("Link expires")).toBeEnabled();
});

test("shows missing, invalid, and unavailable share link errors", async ({ page }) => {
  const id = "12345678-1234-4234-8234-123456789abc";
  await page.goto(`/share/${id}`);
  await expect(page.getByText("This share link has an invalid key.")).toBeVisible();
  // Changing only the fragment reloads the share view. Wait for that reload:
  // the old view displays the same error and can otherwise satisfy the assertion.
  const reload = page.waitForEvent("load");
  await page.goto(`/share/${id}#invalid`);
  await reload;
  await expect(page.getByText("This share link has an invalid key.")).toBeVisible();
  await page.goto(`/share/${id}#${"A".repeat(43)}`);
  await expect(page.getByText("This file has expired or is no longer available.")).toBeVisible();
});

test("mobile-sized Firefox layout supports choosing a file without horizontal overflow", async ({
  page,
}) => {
  await page.setViewportSize({ width: 390, height: 844 });
  await page.goto("/");
  await expect(page.getByText("Browse files")).toBeVisible();
  await expect
    .poll(() => page.evaluate(() => document.documentElement.scrollWidth <= innerWidth))
    .toBe(true);
  await page
    .locator("#file-input")
    .setInputFiles({ name: "mobile.txt", mimeType: "text/plain", buffer: Buffer.from("hello") });
  await expect(page.getByText("mobile.txt")).toBeVisible();
  await expect(page.getByRole("button", { name: "Create a private link" })).toBeEnabled();
});

test("front page animates without a motion toggle", async ({ page }) => {
  await page.goto("/");
  await expect(page.getByRole("button", { name: /animations/i })).toHaveCount(0);
  await expect(page.locator(".eyebrow")).toHaveCount(0);
  for (const selector of [
    ".transfer-card",
    ".courier",
    ".letter",
    ".galaxy-spin",
    ".shockwave",
    ".art-star",
  ]) {
    await expect(page.locator(selector).first()).not.toHaveCSS("animation-name", "none");
  }
});

test("front page is still when reduced motion is requested", async ({ page }) => {
  await page.emulateMedia({ reducedMotion: "reduce" });
  await page.goto("/");
  for (const selector of [
    ".brand",
    ".transfer-card",
    ".courier",
    ".letter",
    ".galaxy",
    ".galaxy-spin",
    ".art-star",
  ]) {
    for (const element of await page.locator(selector).all()) {
      await expect(element).toHaveCSS("animation-name", "none");
      await expect(element).toHaveCSS("transition-duration", "0s");
    }
  }
});
