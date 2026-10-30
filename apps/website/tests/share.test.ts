import { afterEach, beforeEach, expect, test, vi } from "vite-plus/test";
import { MAX_FILE_SIZE, openShare, type DownloadProgress } from "../src/share.ts";
import { timing } from "../src/retry.ts";
import { createShare, type ShareProgress } from "../src/upload.ts";

type Reply = { status: number; body?: string; type?: string; retryAfter?: string };
// "network-error" fails a request before the server sees it; "lost" loses the reply after.
type Outcome = Reply | "network-error" | "lost" | undefined;
type Upload = { id: string; length: number; parts: Map<number, Uint8Array> };

const json = (status: number, body: unknown): Reply => ({
  status,
  body: JSON.stringify(body),
  type: "application/json",
});

// Stands in for the server's upload API.
class FakeServer {
  partSize = 64 * 1024;
  uploads = new Map<string, Upload>();
  completed = new Map<string, string>();
  files = new Map<string, Uint8Array<ArrayBuffer>>();
  created: Array<{ length: number; expiry: string | undefined }> = [];
  log: string[] = [];
  private counts = new Map<string, number>();
  // Lets a test change how a request is handled, given how often it has been made.
  intercept: (method: string, path: string, attempt: number) => Outcome = () => undefined;

  count(request: string) {
    return this.counts.get(request) ?? 0;
  }

  handle(
    method: string,
    path: string,
    headers: Record<string, string>,
    body: Uint8Array | null,
  ): Reply | "network-error" {
    const request = `${method} ${path}`;
    const attempt = this.count(request) + 1;
    this.counts.set(request, attempt);
    this.log.push(request);
    const outcome = this.intercept(method, path, attempt);
    if (outcome === "network-error") return outcome;
    const reply =
      outcome === "lost" || !outcome ? this.respond(method, path, headers, body) : outcome;
    return outcome === "lost" ? "network-error" : reply;
  }

  private respond(
    method: string,
    path: string,
    headers: Record<string, string>,
    body: Uint8Array | null,
  ): Reply {
    if (method === "POST" && path === "/api/uploads") {
      const length = Number(headers["x-hokan-upload-length"]);
      const id = crypto.randomUUID();
      const token = `${id}.${length}.${"ab".repeat(16)}`;
      this.created.push({ length, expiry: headers["x-hokan-expiry-seconds"] });
      this.uploads.set(token, { id, length, parts: new Map() });
      return json(201, { upload: token, part_size: this.partSize });
    }
    const [, token, action, number] =
      /^\/api\/uploads\/([^/]+)(?:\/(complete|parts)(?:\/(\d+))?)?$/.exec(path) ?? [];
    if (!token) return { status: 404 };
    const upload = this.uploads.get(token);
    if (method === "DELETE") {
      this.uploads.delete(token);
      return { status: 204 };
    }
    if (method === "POST" && action === "complete") {
      const done = this.completed.get(token);
      if (done) return json(201, { id: done, expires_in_seconds: 86400 });
      if (!upload) return { status: 404, body: "Upload not found", type: "text/plain" };
      const bytes = new Uint8Array(upload.length);
      for (let part = 1; part <= Math.ceil(upload.length / this.partSize); part++) {
        const received = upload.parts.get(part);
        if (!received) return { status: 409, body: "Upload is missing parts", type: "text/plain" };
        bytes.set(received, (part - 1) * this.partSize);
      }
      this.files.set(upload.id, bytes);
      this.uploads.delete(token);
      this.completed.set(token, upload.id);
      return json(201, { id: upload.id, expires_in_seconds: 86400 });
    }
    if (!upload) return { status: 404, body: "Upload not found", type: "text/plain" };
    if (method === "GET" && !action)
      return json(200, {
        part_size: this.partSize,
        received: [...upload.parts.keys()].sort((a, b) => a - b),
      });
    if (method === "PUT" && action === "parts") {
      const part = Number(number);
      const expected = Math.min(this.partSize, upload.length - (part - 1) * this.partSize);
      if (headers["content-type"] !== "application/octet-stream" || body?.length !== expected)
        return { status: 400, body: "Part has the wrong size", type: "text/plain" };
      upload.parts.set(part, body.slice());
      return { status: 204 };
    }
    return { status: 405 };
  }
}

function install(server: FakeServer) {
  class FakeRequest {
    private method = "";
    private url = "";
    private headers: Record<string, string> = {};
    private replyType: string | null = null;
    private retryAfter: string | null = null;
    private aborted = false;
    status = 0;
    responseText = "";
    upload: { onprogress?: (event: { loaded: number }) => void; onload?: () => void } = {};
    onload?: () => void;
    onerror?: () => void;
    ontimeout?: () => void;
    onabort?: () => void;
    open(method: string, url: string) {
      this.method = method;
      this.url = url;
    }
    setRequestHeader(name: string, value: string) {
      this.headers[name.toLowerCase()] = value;
    }
    getResponseHeader(name: string) {
      if (name.toLowerCase() === "content-type") return this.replyType;
      return name.toLowerCase() === "retry-after" ? this.retryAfter : null;
    }
    abort() {
      if (this.aborted) return;
      this.aborted = true;
      queueMicrotask(() => this.onabort?.());
    }
    send(body: Uint8Array | null) {
      setTimeout(() => {
        if (this.aborted) return;
        const reply = server.handle(this.method, this.url, this.headers, body);
        if (reply === "network-error") return this.onerror?.();
        if (body) {
          this.upload.onprogress?.({ loaded: body.length });
          this.upload.onload?.();
        }
        this.status = reply.status;
        this.responseText = reply.body ?? "";
        this.replyType = reply.type ?? null;
        this.retryAfter = reply.retryAfter ?? null;
        this.onload?.();
      });
    }
  }
  vi.stubGlobal("XMLHttpRequest", FakeRequest);
  vi.stubGlobal(
    "fetch",
    vi.fn(async (url: string) => {
      const bytes = server.files.get(url.slice("/api/files/".length));
      return bytes ? new Response(bytes) : new Response(null, { status: 404 });
    }),
  );
}

async function open(link: string) {
  vi.stubGlobal("location", new URL(link));
  return openShare();
}

// Comparing digests is much faster than comparing megabytes element by element.
const digest = async (bytes: BufferSource) =>
  new Uint8Array(await crypto.subtle.digest("SHA-256", bytes));

const patterned = (length: number) => Uint8Array.from({ length }, (_, index) => index % 251);

beforeEach(() => {
  timing.retryBase = 1;
  timing.retryMax = 4;
  vi.stubGlobal("isSecureContext", true);
  vi.stubGlobal("location", new URL("https://example.test/"));
});
afterEach(() => vi.unstubAllGlobals());

test("shares an encrypted file with the key only in the fragment and decrypts it", async () => {
  const server = new FakeServer();
  install(server);
  const file = new File(["a private message"], "secret.txt", { type: "text/plain" });
  const progress: ShareProgress[] = [];
  const link = await createShare(file, 86400, { onProgress: (event) => progress.push(event) });
  expect(link).toMatch(/^https:\/\/example.test\/share\/[0-9a-f-]{36}#[A-Za-z0-9_-]{43}$/);
  expect(server.created).toEqual([{ length: expect.any(Number), expiry: "86400" }]);
  expect(progress[0]).toEqual({ stage: "preparing" });
  expect(progress.at(-1)).toEqual({ stage: "saving" });
  const total = server.created[0]!.length;
  expect(progress).toContainEqual({ stage: "uploading", loaded: total, total });
  const [stored] = server.files.values();
  const text = new TextDecoder().decode(stored);
  expect(text).not.toContain("a private message");
  expect(text).not.toContain("secret.txt");
  expect(text).not.toContain("text/plain");
  const result = await open(link);
  expect(result?.name).toBe("secret.txt");
  expect(result?.type).toBe("text/plain");
  // The download blob never carries the sender's type, since blob URLs run in this origin.
  expect(result?.blob.type).toBe("application/octet-stream");
  expect(await result?.blob.text()).toBe("a private message");

  stored![stored!.length - 1]! ^= 1;
  await expect(openShare()).rejects.toThrow("Could not decrypt");
});

test("falls back to application/octet-stream when the file type is unknown", async () => {
  install(new FakeServer());
  const link = await createShare(new File(["data"], "data"), 86400);
  expect((await open(link))?.type).toBe("application/octet-stream");
});

test("removes filename direction controls before sharing and downloading", async () => {
  install(new FakeServer());
  const link = await createShare(new File(["data"], "invoice‮fdp.exe"), 86400);
  expect((await open(link))?.name).toBe("invoice_fdp.exe");
});

test("encrypts large files in chunks, sends equal parts and detects a missing chunk", async () => {
  const server = new FakeServer();
  server.partSize = 1024 * 1024;
  install(server);
  const contents = patterned(2.5 * 1024 * 1024);
  const link = await createShare(new File([contents], "large.bin"), 86400);
  const [stored] = server.files.values();
  // A version byte and nonce prefix, then three chunks that each carry a 16-byte tag.
  expect(stored![0]).toBe(2);
  const metadataLength = JSON.stringify({
    name: "large.bin",
    type: "application/octet-stream",
  }).length;
  expect(stored!.length).toBe(8 + 4 + metadataLength + contents.length + 3 * 16);
  expect(server.log.filter((request) => request.startsWith("PUT"))).toHaveLength(3);
  const result = await open(link);
  expect(await digest(await result!.blob.arrayBuffer())).toEqual(await digest(contents));

  // Dropping the last chunk leaves a chunk that was not sealed as the last one.
  const id = new URL(link).pathname.split("/").at(-1)!;
  server.files.set(id, stored!.slice(0, 8 + 2 * (1024 * 1024 + 16)));
  await expect(openShare()).rejects.toThrow("Could not decrypt");
});

test("retries failed requests until the upload goes through", async () => {
  const server = new FakeServer();
  server.partSize = 4096;
  server.intercept = (method, path, attempt) => {
    if (path === "/api/uploads" && attempt === 1) return { status: 502 };
    if (method === "PUT" && attempt === 1) return "network-error";
    if (method === "PUT" && attempt === 2) return { status: 503, retryAfter: "0" };
    // The server completes the upload, but the reply never arrives.
    if (path.endsWith("/complete") && attempt === 1) return "lost";
    return undefined;
  };
  install(server);
  const contents = patterned(20_000);
  const progress: ShareProgress[] = [];
  const link = await createShare(new File([contents], "flaky.bin"), 86400, {
    onProgress: (event) => progress.push(event),
  });
  expect(await digest(await (await open(link))!.blob.arrayBuffer())).toEqual(
    await digest(contents),
  );
  expect(progress.some((event) => event.stage === "reconnecting")).toBe(true);
  const parts = server.log.filter((request) => request.startsWith("PUT"));
  // Five parts, each tried three times.
  expect(parts).toHaveLength(15);
  expect(server.created).toHaveLength(1);
});

test("starts over when the server no longer has the upload", async () => {
  const server = new FakeServer();
  server.partSize = 4096;
  let restarted = false;
  server.intercept = (method, path) => {
    if (method === "PUT" && path.endsWith("/parts/2") && !restarted) {
      // As after a restart of a server that keeps files in memory.
      restarted = true;
      server.uploads.clear();
      return { status: 404, body: "Upload not found", type: "text/plain" };
    }
    return undefined;
  };
  install(server);
  const contents = patterned(10_000);
  const link = await createShare(new File([contents], "restart.bin"), 86400);
  expect(server.created).toHaveLength(2);
  expect(await digest(await (await open(link))!.blob.arrayBuffer())).toEqual(
    await digest(contents),
  );
});

test("sends only the parts the server is missing", async () => {
  const server = new FakeServer();
  server.partSize = 4096;
  server.intercept = (method, path, attempt) => {
    if (path.endsWith("/complete") && attempt === 1) {
      for (const upload of server.uploads.values()) upload.parts.delete(1);
    }
    return undefined;
  };
  install(server);
  const contents = patterned(10_000);
  const link = await createShare(new File([contents], "gap.bin"), 86400);
  const token = [...server.completed.keys()][0];
  expect(server.count(`PUT /api/uploads/${token}/parts/1`)).toBe(2);
  expect(server.count(`PUT /api/uploads/${token}/parts/2`)).toBe(1);
  expect(server.count(`PUT /api/uploads/${token}/parts/3`)).toBe(1);
  expect(await digest(await (await open(link))!.blob.arrayBuffer())).toEqual(
    await digest(contents),
  );
});

test("refuses to encrypt a chunk again if the file has changed", async () => {
  const server = new FakeServer();
  server.partSize = 4096;
  const original = patterned(10_000);
  const changed = original.map((byte) => byte ^ 1);
  let readChanged = false;
  const file = new File([original], "changing.bin");
  file.slice = (start, end) => new Blob([readChanged ? changed : original]).slice(start, end);
  server.intercept = (method, path, attempt) => {
    if (path.endsWith("/complete") && attempt === 1) {
      readChanged = true;
      for (const upload of server.uploads.values()) upload.parts.delete(1);
    }
    return undefined;
  };
  install(server);
  await expect(createShare(file, 86400)).rejects.toThrow("The file changed");
  expect(server.log.at(-1)).toMatch(/^DELETE /);
  expect(server.files.size).toBe(0);
});

test("cancelling stops the upload and tells the server", async () => {
  const server = new FakeServer();
  server.partSize = 4096;
  const controller = new AbortController();
  server.intercept = (method) => {
    if (method === "PUT") controller.abort();
    return undefined;
  };
  install(server);
  const upload = createShare(new File([patterned(10_000)], "cancel.bin"), 86400, {
    signal: controller.signal,
  });
  await expect(upload).rejects.toMatchObject({ name: "AbortError" });
  expect(server.log.at(-1)).toMatch(/^DELETE /);
  expect(server.uploads.size).toBe(0);
});

// Shares `contents` and returns the link with the stored envelope.
async function sharedEnvelope(contents: Uint8Array<ArrayBuffer>) {
  const server = new FakeServer();
  install(server);
  const link = await createShare(new File([contents], "resume.bin"), 86400);
  const [envelope] = server.files.values();
  return { link, envelope: envelope! };
}

// A response that sends the first `cut` bytes of the file and then loses its connection.
const cutOff = (envelope: Uint8Array<ArrayBuffer>, cut: number) => {
  let sent = false;
  return new Response(
    new ReadableStream<Uint8Array>({
      pull(controller) {
        if (sent) return controller.error(new TypeError("Connection reset"));
        sent = true;
        controller.enqueue(envelope.slice(0, cut));
      },
    }),
    { headers: { "content-length": String(envelope.length) } },
  );
};

const rest = (envelope: Uint8Array<ArrayBuffer>, start: number, total = envelope.length) =>
  new Response(envelope.slice(start), {
    status: 206,
    headers: { "content-range": `bytes ${start}-${envelope.length - 1}/${total}` },
  });

const rangeOf = (init?: RequestInit) =>
  (init?.headers as Record<string, string> | undefined)?.Range ?? null;

test("continues a download from where the connection failed", async () => {
  const contents = patterned(3 * 1024 * 1024);
  const { link, envelope } = await sharedEnvelope(contents);
  const cut = 1_500_000;
  const ranges: Array<string | null> = [];
  vi.stubGlobal(
    "fetch",
    vi.fn(async (_: string, init?: RequestInit) => {
      ranges.push(rangeOf(init));
      if (ranges.length === 1) return cutOff(envelope, cut);
      // The server is briefly unavailable before the download can continue.
      if (ranges.length === 2) return new Response(null, { status: 503 });
      return rest(envelope, cut);
    }),
  );
  vi.stubGlobal("location", new URL(link));
  const progress: DownloadProgress[] = [];
  const result = await openShare((event) => progress.push(event));
  expect(ranges).toEqual([null, `bytes=${cut}-`, `bytes=${cut}-`]);
  expect(progress.filter((event) => event.stage === "reconnecting")).toHaveLength(2);
  expect(progress.at(-1)).toEqual({
    stage: "downloading",
    loaded: envelope.length,
    total: envelope.length,
  });
  expect(await digest(await result!.blob.arrayBuffer())).toEqual(await digest(contents));
});

test("skips what it already has when the server ignores the range", async () => {
  const contents = patterned(3 * 1024 * 1024);
  const { link, envelope } = await sharedEnvelope(contents);
  let calls = 0;
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => (++calls === 1 ? cutOff(envelope, 1_000_003) : new Response(envelope))),
  );
  vi.stubGlobal("location", new URL(link));
  const result = await openShare();
  expect(calls).toBe(2);
  expect(await digest(await result!.blob.arrayBuffer())).toEqual(await digest(contents));
});

test("refuses to continue a download with a different file", async () => {
  const { link, envelope } = await sharedEnvelope(patterned(3 * 1024 * 1024));
  let calls = 0;
  vi.stubGlobal(
    "fetch",
    vi.fn(async () =>
      ++calls === 1 ? cutOff(envelope, 1_000_000) : rest(envelope, 1_000_000, envelope.length + 1),
    ),
  );
  vi.stubGlobal("location", new URL(link));
  await expect(openShare()).rejects.toThrow("invalid or unsupported");
});

test("opens version 1 links", async () => {
  const rawKey = crypto.getRandomValues(new Uint8Array(32));
  const iv = crypto.getRandomValues(new Uint8Array(12));
  const metadata = new TextEncoder().encode(
    JSON.stringify({ name: "old.txt", type: "text/plain" }),
  );
  const plaintext = new Uint8Array([0, 0, 0, metadata.length, ...metadata, 104, 105]);
  const key = await crypto.subtle.importKey("raw", rawKey, "AES-GCM", false, ["encrypt"]);
  const sealed = new Uint8Array(
    await crypto.subtle.encrypt({ name: "AES-GCM", iv }, key, plaintext),
  );
  const id = "12345678-1234-4234-8234-123456789abc";
  const fragment = btoa(String.fromCharCode(...rawKey))
    .replaceAll("+", "-")
    .replaceAll("/", "_")
    .replace(/=+$/, "");
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => new Response(new Uint8Array([1, ...iv, ...sealed]))),
  );
  const result = await open(`https://example.test/share/${id}#${fragment}`);
  expect(result?.name).toBe("old.txt");
  expect(await result?.blob.text()).toBe("hi");
});

test("stops reading an oversized encrypted response", async () => {
  const id = "12345678-1234-4234-8234-123456789abc";
  let cancelled = false;
  // Version 1 envelopes are limited to 25 MiB.
  const first = new Uint8Array(13 * 1024 * 1024);
  first[0] = 1;
  const body = new ReadableStream<Uint8Array>({
    start(controller) {
      controller.enqueue(first);
      controller.enqueue(new Uint8Array(13 * 1024 * 1024));
    },
    cancel() {
      cancelled = true;
    },
  });
  vi.stubGlobal(
    "fetch",
    vi.fn(async () => new Response(body)),
  );
  await expect(open(`https://example.test/share/${id}#${"A".repeat(43)}`)).rejects.toThrow(
    "invalid or unsupported",
  );
  expect(cancelled).toBe(true);
});

test("sends 0 seconds when the link should never expire", async () => {
  const server = new FakeServer();
  install(server);
  await createShare(new File(["hello"], "hello.txt"), 0);
  expect(server.created[0]?.expiry).toBe("0");
});

test("rejects oversized files before upload", async () => {
  const request = vi.fn();
  vi.stubGlobal("XMLHttpRequest", request);
  const oversized = new File([], "large.bin");
  Object.defineProperty(oversized, "size", { value: MAX_FILE_SIZE + 1 });
  await expect(createShare(oversized, 86400)).rejects.toThrow("5 GiB");
  expect(request).not.toHaveBeenCalled();
});

test("explains why an upload failed", async () => {
  const file = new File(["hello"], "hello.txt");
  const failWith = async (reply: Reply, on = "/api/uploads") => {
    const server = new FakeServer();
    server.intercept = (_, path) => (path.endsWith(on) ? reply : undefined);
    install(server);
    return createShare(file, 86400);
  };

  await expect(failWith({ status: 405 })).rejects.toThrow("Restart it with the latest version");
  await expect(failWith({ status: 413 })).rejects.toThrow("larger than the server accepts");
  await expect(
    failWith({ status: 507, body: "Storage capacity reached", type: "text/plain; charset=utf-8" }),
  ).rejects.toThrow("(Storage capacity reached)");
  await expect(failWith({ status: 400 })).rejects.toThrow("(error 400)");
  // A daily limit that lasts for hours is not waited out.
  await expect(failWith({ status: 429, retryAfter: "86400" })).rejects.toThrow(
    "daily upload limit",
  );
  await expect(failWith(json(201, { nope: true }))).rejects.toThrow("invalid upload");
  await expect(
    failWith({ status: 201, body: "<html>not json</html>", type: "text/html" }, "/complete"),
  ).rejects.toThrow("invalid share ID");
});
